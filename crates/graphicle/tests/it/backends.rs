use axum::{Json, Router, extract::State, http::HeaderMap, routing::post};
use graphicle_runtime::{
    distributed, graph,
    jobs::Worker,
    local, model, query, settings,
    store::{Store, repo, s},
    vector,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

struct Fixture {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    store: Store,
    ws: String,
    key: String,
    project: String,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("source");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("rules.md"), "Ship only after review.\n").unwrap();
        let store = Store::open(dir.path().join("store")).unwrap();
        let mut session = local::session(&store, true).unwrap();
        let synced = local::sync(&store, &mut session, &root, None).unwrap();
        Self {
            root,
            ws: session.workspace,
            key: session.capability,
            project: s(&synced, "repository_id").into(),
            store,
            _dir: dir,
        }
    }
    fn args(&self, mut value: Value) -> Value {
        value["repository_id"] = json!(self.project);
        value
    }
    fn sync(&self) {
        let mut session = local::Session {
            workspace: self.ws.clone(),
            capability: self.key.clone(),
            project: Some(self.project.clone()),
        };
        local::sync(&self.store, &mut session, &self.root, Some(&self.project)).unwrap();
    }
}
#[derive(Clone, Default)]
struct Replies {
    requests: Arc<Mutex<Vec<Value>>>,
    responses: Arc<Mutex<Vec<Value>>>,
}
async fn transport(
    State(state): State<Replies>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    state.requests.lock().unwrap().push(json!({"body":body,"authorization":headers.get("authorization").and_then(|v|v.to_str().ok()),"x-api-key":headers.get("x-api-key").and_then(|v|v.to_str().ok()),"x-goog-api-key":headers.get("x-goog-api-key").and_then(|v|v.to_str().ok())}));
    Json(state.responses.lock().unwrap().remove(0))
}
async fn server(responses: Vec<Value>) -> (String, Replies, tokio::task::JoinHandle<()>) {
    let state = Replies {
        responses: Arc::new(Mutex::new(responses)),
        ..Replies::default()
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new()
        .route("/messages", post(transport))
        .route("/models/fixture:generateContent", post(transport))
        .route("/chat/completions", post(transport))
        .with_state(state.clone());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (base, state, task)
}
fn profile(protocol: &str, base: &str) -> model::Profile {
    serde_json::from_value(json!({"protocol":protocol,"base_url":base,"model":"fixture","input_limit":4000,"output_limit":1000,"temperature":0})).unwrap()
}
fn config(profile: model::Profile) -> model::Config {
    model::Config {
        default_model: Some("fixture".into()),
        embedding_model: Some("fixture".into()),
        models: [("fixture".into(), profile)].into(),
        ..Default::default()
    }
}
#[tokio::test]
async fn native_anthropic_and_gemini_transports_count_usage_and_reject_truncation() {
    let schema = json!({"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false});
    let(base,requests,task)=server(vec![json!({"stop_reason":"tool_use","content":[{"type":"tool_use","name":"graphicle_result","input":{"answer":"ready"}}],"usage":{"input_tokens":12,"cache_read_input_tokens":20,"output_tokens":4}}),json!({"candidates":[{"finishReason":"STOP","content":{"parts":[{"thought":true,"text":"hidden reasoning"},{"text":"{\"answer\":\"ready\"}"}]}}],"usageMetadata":{"promptTokenCount":15,"candidatesTokenCount":4,"thoughtsTokenCount":8}}),json!({"candidates":[{"finishReason":"MAX_TOKENS","content":{"parts":[{"text":"{}"}]}}]})]).await;
    let client = model::client().unwrap();
    let anthropic = profile("anthropic", &base);
    let gemini = profile("gemini", &base);
    let a = model::generate_with_credential(
        &client,
        &anthropic,
        "Use sources",
        &json!({}),
        &schema,
        100,
        Some("fixture-key"),
    )
    .await
    .unwrap();
    assert_eq!(a.json().unwrap()["answer"], "ready");
    assert_eq!(a.input_tokens, 32);
    let b = model::generate_with_credential(
        &client,
        &gemini,
        "Use sources",
        &json!({}),
        &schema,
        100,
        Some("fixture-key"),
    )
    .await
    .unwrap();
    assert_eq!(b.json().unwrap()["answer"], "ready");
    assert_eq!(b.output_tokens, 12);
    assert!(
        model::generate_with_credential(
            &client,
            &gemini,
            "Use sources",
            &json!({}),
            &schema,
            100,
            None
        )
        .await
        .is_err()
    );
    let calls = requests.requests.lock().unwrap();
    assert_eq!(calls[0]["x-api-key"], "fixture-key");
    assert!(calls[0]["authorization"].is_null());
    assert_eq!(calls[1]["x-goog-api-key"], "fixture-key");
    assert_eq!(
        calls[1]["body"]["generationConfig"]["responseMimeType"],
        "application/json"
    );
    task.abort();
}
#[tokio::test]
async fn cut_off_replies_are_rejected_and_never_sent_back() {
    // A provider that closes a stream early still returns well-formed text; only a
    // reply with a normal stop is kept, and the next attempt starts from the prompt.
    let schema = json!({"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false});
    let cut = "{\"answer\":\"46 piles\"}";
    let (base, requests, task) = server(vec![
        json!({"candidates":[{"finishReason":"OTHER","content":{"parts":[{"text":cut}]}}]}),
        json!({"candidates":[{"content":{"parts":[{"text":cut}]}}]}),
        json!({"choices":[{"finish_reason":null,"message":{"content":cut}}]}),
        json!({"choices":[{"finish_reason":"length","message":{"content":cut}}]}),
        json!({"choices":[{"finish_reason":"stop","message":{"content":"{\"answer\":\"plan confirmed\"}"}}],"usage":{"prompt_tokens":9,"completion_tokens":5}}),
    ])
    .await;
    let client = model::client().unwrap();
    let (gemini, chat) = (profile("gemini", &base), profile("chat", &base));
    for (attempt, p) in [&gemini, &gemini, &chat, &chat].into_iter().enumerate() {
        let reply = model::generate_with_credential(
            &client,
            p,
            "Use sources",
            &json!({}),
            &schema,
            100,
            Some("fixture-key"),
        )
        .await;
        assert!(reply.is_err(), "cut-off reply {attempt} was kept");
    }
    let complete = model::generate_with_credential(
        &client,
        &chat,
        "Use sources",
        &json!({}),
        &schema,
        100,
        Some("fixture-key"),
    )
    .await
    .unwrap();
    assert_eq!(
        complete.json_for(&schema).unwrap()["answer"],
        "plan confirmed"
    );
    let calls = requests.requests.lock().unwrap();
    assert_eq!(calls.len(), 5);
    assert_eq!(calls[0]["body"], calls[1]["body"]);
    assert!(calls[2..].iter().all(|c| c["body"] == calls[4]["body"]));
    assert!(
        calls
            .iter()
            .all(|c| !c["body"].to_string().contains("46 piles"))
    );
    drop(calls);
    task.abort();
}
#[tokio::test]
async fn chat_extra_body_reaches_local_servers_without_overriding_the_contract() {
    let schema = json!({"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false});
    let reply = "<think>\nThe notes say ready.\n</think>\n\n{\"answer\":\"ready\"}";
    let (base, requests, task) = server(vec![json!({"choices":[{"finish_reason":"stop","message":{"content":reply}}],"usage":{"prompt_tokens":9,"completion_tokens":5}})]).await;
    let plain = profile("chat", &base);
    let extra = json!({"chat_template_kwargs":{"enable_thinking":false},"top_k":20,"seed":7});
    let mut local = plain.clone();
    local.extra_body = Some(extra.as_object().unwrap().clone());
    config(local.clone()).validate().unwrap();
    // Profiles without request fields keep the identity they had before the field existed.
    assert_eq!(
        model::key(&plain),
        graphicle_runtime::digest(
            &json!({"protocol":"chat","base_url":base,"model":"fixture","temperature":0.0,
            "input_limit":4000,"output_limit":1000,"structured_output":true})
        )
    );
    assert_ne!(model::key(&plain), model::key(&local));
    let r = model::generate_with_credential(
        &model::client().unwrap(),
        &local,
        "Use sources",
        &json!({}),
        &schema,
        100,
        Some("fixture-key"),
    )
    .await
    .unwrap();
    assert_eq!(r.json_for(&schema).unwrap()["answer"], "ready");
    let calls = requests.requests.lock().unwrap();
    let body = &calls[0]["body"];
    assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
    assert_eq!(
        (body["top_k"].clone(), body["seed"].clone()),
        (json!(20), json!(7))
    );
    assert_eq!(body["max_completion_tokens"], 100);
    assert_eq!(body["response_format"]["type"], "json_schema");
    drop(calls);
    task.abort();
    for (protocol, extra) in [
        ("chat", json!({"max_tokens":100000})),
        ("chat", json!({"response_format":{"type":"text"}})),
        ("chat", json!({"temperature":1.5})),
        ("chat", json!({"messages":[]})),
        ("chat", json!({"padding":"x".repeat(5000)})),
        ("anthropic", json!({"top_k":20})),
        ("ollama", json!({"top_k":20})),
    ] {
        let mut p = profile(protocol, "http://127.0.0.1:1");
        p.extra_body = Some(extra.as_object().unwrap().clone());
        assert!(config(p).validate().is_err(), "{protocol} {extra}");
    }
}
#[test]
fn workspace_model_settings_are_live_and_isolated() {
    let f = Fixture::new();
    let base = model::Config::default();
    let key = f
        .store
        .put_credential(
            &f.ws,
            &f.key,
            &json!({"name":"api","value":"fixture-provider-key"}),
        )
        .unwrap();
    settings::configure(&f.store,&f.ws,&f.key,&base,&json!({"name":"mine","profile":{"protocol":"anthropic","base_url":"https://api.anthropic.com/v1","model":"fixture","credential_ref":key["credential_ref"]},"default":true})).unwrap();
    assert_eq!(
        settings::status(&f.store, &f.ws, &f.key, &base).unwrap()["models"][0]["credentials_available"],
        true
    );
    let other = f.store.create_workspace("Other").unwrap();
    assert!(
        settings::configuration(&f.store, s(&other, "id"), &base)
            .unwrap()
            .models
            .is_empty()
    );
    let cfg = settings::configuration(&f.store, &f.ws, &base).unwrap();
    assert_eq!(cfg.default_model.as_deref(), Some("mine"));
    let queued = f
        .store
        .enqueue(
            &f.ws,
            &f.key,
            &f.args(json!({"kind":"basic","question":"What should we do?"})),
            &base,
        )
        .unwrap();
    assert_eq!(queued["state"], "queued");
}
#[tokio::test]
#[ignore = "Requires isolated Neo4j on 127.0.0.1:17474"]
async fn real_neo4j_roundtrip_revalidates_deleted_sources() {
    let f = Fixture::new();
    let secret=f.store.put_credential(&f.ws,&f.key,&json!({"name":"neo4j fixture","value":json!({"username":"neo4j","password":"graphicle-fixture-password"}).to_string()})).unwrap();
    graph::configure_neo4j(
        &f.store,
        &f.ws,
        &f.key,
        &f.args(
            json!({"endpoint":"http://127.0.0.1:17474","credential_ref":secret["credential_ref"]}),
        ),
    )
    .unwrap();
    let client = model::client().unwrap();
    let published = graph::neo4j(&f.store, &client, &f.ws, &f.key, &f.args(json!({})), true)
        .await
        .unwrap();
    assert_eq!(published["nodes"], 1);
    let found = graph::neo4j(
        &f.store,
        &client,
        &f.ws,
        &f.key,
        &f.args(json!({"query":"rules"})),
        false,
    )
    .await
    .unwrap();
    assert_eq!(found["nodes"].as_array().unwrap().len(), 1);
    std::fs::remove_file(f.root.join("rules.md")).unwrap();
    f.sync();
    let found = graph::neo4j(
        &f.store,
        &client,
        &f.ws,
        &f.key,
        &f.args(json!({"query":"rules"})),
        false,
    )
    .await
    .unwrap();
    assert!(found["nodes"].as_array().unwrap().is_empty());
    let pending = graph::neo4j(&f.store, &client, &f.ws, &f.key, &f.args(json!({})), true)
        .await
        .unwrap_err();
    assert_eq!(pending.status, 409);
    assert!(
        graphicle_runtime::projection::once(&f.store, &client)
            .await
            .unwrap()
    );
    let remote: Value = client
        .post("http://127.0.0.1:17474/db/neo4j/tx/commit")
        .basic_auth("neo4j", Some("graphicle-fixture-password"))
        .json(&json!({"statements":[{"statement":"MATCH (n:Graphicle {scope:$scope}) RETURN count(n)","parameters":{"scope":f.project}}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(remote["errors"].as_array().unwrap().is_empty());
    assert_eq!(remote["results"][0]["data"][0]["row"][0], 0);
    graph::neo4j(&f.store, &client, &f.ws, &f.key, &f.args(json!({})), true)
        .await
        .unwrap();
}
#[tokio::test]
#[ignore = "Requires isolated Qdrant on 127.0.0.1:16333"]
async fn real_qdrant_filters_fingerprints_and_authorized_candidate_ids() {
    let f = Fixture::new();
    let collection = format!("graphicle_{}", graphicle_runtime::id());
    let profile = profile("chat", "http://127.0.0.1:1");
    let key = model::key(&profile);
    let cfg = config(profile);
    let worker = Worker::new(f.store.clone(), cfg.clone()).unwrap();
    vector::configure(
        &f.store,
        &f.ws,
        &f.key,
        &f.args(json!({"endpoint":"http://127.0.0.1:16333","collection":collection})),
    )
    .unwrap();
    let candidates = f
        .store
        .authorized(&f.ws, &f.key, "read", |db, p| {
            let r = repo(db, p, &f.project)?;
            query::candidates(&query::snapshot(db, &r, p)?).map(|v| v.0)
        })
        .unwrap();
    for row in &candidates {
        f.store
            .connect()
            .unwrap()
            .execute(
                "INSERT INTO am_vectors VALUES(?,?,?,?,?,?)",
                rusqlite::params![
                    f.project,
                    s(row, "identity"),
                    s(row, "source_sha"),
                    key,
                    2,
                    "[1.0,0.0]"
                ],
            )
            .unwrap();
    }
    let result = vector::publish(
        &f.store,
        &worker.client,
        &f.ws,
        &f.key,
        &f.args(json!({"model_key":key})),
    )
    .await
    .unwrap();
    assert_eq!(result["published"], candidates.len());
    f.store
        .enqueue(
            &f.ws,
            &f.key,
            &f.args(json!({"kind":"recall","question":"rules"})),
            &cfg,
        )
        .unwrap();
    let job = f.store.lease_job().unwrap().unwrap();
    let ranked = vector::ranking(&worker, &job, &candidates, &[1.0, 0.0], &key)
        .await
        .unwrap();
    assert_eq!(ranked["semantic"][0], candidates[0]["identity"]);
    let none = vector::ranking(&worker, &job, &[], &[1.0, 0.0], &key)
        .await
        .unwrap();
    assert!(none["semantic"].as_array().unwrap().is_empty());
    let mut altered = candidates.clone();
    altered[0]["source_sha"] = json!("different-revision");
    let none = vector::ranking(&worker, &job, &altered, &[1.0, 0.0], &key)
        .await
        .unwrap();
    assert!(none["semantic"].as_array().unwrap().is_empty());
    worker
        .client
        .delete(format!("http://127.0.0.1:16333/collections/{collection}"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
}
#[tokio::test]
#[ignore = "Requires isolated Redis/Valkey on 127.0.0.1:16379"]
async fn real_distributed_consumer_returns_grounded_result_without_workspace_capability() {
    let f = Fixture::new();
    let answer = json!({"answer":"Ship only after review.","abstain":false,"evidence":[{"id":"file:rules.md","quote":"Ship only after review."}],"unresolved":[]});
    let(base,requests,server)=server(vec![json!({"choices":[{"finish_reason":"stop","message":{"content":answer.to_string()}}],"usage":{"prompt_tokens":70,"completion_tokens":35}})]).await;
    let profile = profile("chat", &base);
    let cfg = config(profile.clone());
    let worker = Worker::new(f.store.clone(), cfg.clone()).unwrap();
    let secret = f
        .store
        .put_credential(
            &f.ws,
            &f.key,
            &json!({"name":"queue fixture","value":"graphicle-fixture-password"}),
        )
        .unwrap();
    let queue = distributed::Queue {
        endpoint: "redis://127.0.0.1:16379/0".into(),
        namespace: format!("test_{}", graphicle_runtime::id()),
        username: None,
        credential_ref: s(&secret, "credential_ref").into(),
    };
    distributed::configure(&f.store, &f.ws, &f.key, &f.args(json!({"config":queue}))).unwrap();
    let consumer = tokio::spawn(async move {
        distributed::consume(&queue, "graphicle-fixture-password", &profile, None, true)
            .await
            .unwrap()
    });
    let job=f.store.enqueue(&f.ws,&f.key,&f.args(json!({"kind":"basic","question":"When can we ship?","dispatch":"redis","max_calls":1})),&cfg).unwrap();
    assert!(worker.once().await.unwrap());
    let status = f
        .store
        .job_status(&f.ws, &f.key, &f.args(json!({"job_id":job["id"]})))
        .unwrap();
    assert_eq!(status["state"], "complete", "{status}");
    assert_eq!(status["result"]["answer"], "Ship only after review.");
    assert_eq!(consumer.await.unwrap()["processed"], 1);
    let calls = requests.requests.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert!(!calls[0].to_string().contains(&f.key));
    server.abort();
}
