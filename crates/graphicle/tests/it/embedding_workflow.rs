//! G09 integration: source import -> queue -> provider -> usage ledger and vectors.
use axum::{Json, Router, extract::State, routing::post};
use graphicle_memory::text;
use graphicle_runtime::{
    jobs::Worker,
    model, query,
    store::{self, Store, s},
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

type Requests = Arc<Mutex<Vec<Vec<String>>>>;

fn vector(input: &str) -> Vec<f64> {
    vec![
        input.len() as f64,
        input.bytes().map(u64::from).sum::<u64>() as f64,
    ]
}

async fn embeddings(State(requests): State<Requests>, Json(body): Json<Value>) -> Json<Value> {
    let inputs: Vec<String> = serde_json::from_value(body["input"].clone()).unwrap();
    requests.lock().unwrap().push(inputs.clone());
    Json(json!({
        "data":inputs.iter().enumerate().map(|(index, input)|json!({
            "index":index,"embedding":vector(input)
        })).collect::<Vec<_>>(),
        "usage":{"prompt_tokens":inputs.iter().map(|input|text::count(input)).sum::<usize>()}
    }))
}

struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[tokio::test]
async fn imported_duplicate_text_keeps_each_source_vector_and_accounts_only_submitted_work() {
    let directory = tempfile::TempDir::new().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let workspace = store.create_workspace("Embedding qualification").unwrap();
    let ws = s(&workspace, "id");
    let key = s(&workspace, "capability");
    let project = store
        .register(ws, key, &json!({"project_key":"fixture","name":"Fixture"}))
        .unwrap();
    let project_id = s(&project, "id");
    let connector = store
        .connector_add(
            ws,
            key,
            &json!({
                "repository_id":project_id,"kind":"jsonl","name":"Imported notes",
                "config":{"acl":{"workspace_readers":true,"credentials":[]}}
            }),
        )
        .unwrap();
    let documents: Vec<_> = (0..8)
        .map(|i| {
            json!({
                "id":format!("source-{i}"),"title":"Shared note",
                "content":if i % 2 == 0 {"route"} else {"worker"}
            })
        })
        .collect();
    store
        .import_documents(
            ws,
            key,
            &json!({
                "repository_id":project_id,"connector_id":connector["id"],
                "mode":"snapshot","documents":documents
            }),
        )
        .unwrap();

    let requests = Requests::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/v1/embeddings", post(embeddings))
        .with_state(requests.clone());
    let _server = Server(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    }));
    let configuration: model::Config = serde_json::from_value(json!({
        "default_model":"fixture","embedding_model":"fixture","models":{"fixture":{
            "protocol":"chat","base_url":format!("http://{address}/v1"),
            "model":"fixture","input_limit":1000
        }}
    }))
    .unwrap();
    let job = store
        .enqueue(
            ws,
            key,
            &json!({
                "repository_id":project_id,"kind":"embed","max_calls":1
            }),
            &configuration,
        )
        .unwrap();
    assert!(
        Worker::new(store.clone(), configuration)
            .unwrap()
            .once()
            .await
            .unwrap()
    );
    let result = store
        .job_status(
            ws,
            key,
            &json!({
                "repository_id":project_id,"job_id":job["id"]
            }),
        )
        .unwrap();
    assert_eq!(result["state"], "complete", "{result}");
    assert_eq!(result["result"]["embedded_passages"], 8);
    assert_eq!(result["result"]["pending_passages"], 0);
    assert_eq!(result["calls"].as_array().unwrap().len(), 1);
    assert_eq!(result["calls"][0]["usage_reported"], 1);
    assert_eq!(result["calls"][0]["call_state"], "finished");

    // Exercise the authorized source path when checking persisted associations.
    store
        .authorized(ws, key, "read", |db, principal| {
            let repository = store::repo(db, principal, project_id)?;
            let snapshot = query::snapshot(db, &repository, principal)?;
            let (candidates, _) = query::candidates(&snapshot)?;
            let vectors = store::rows(
                db,
                "SELECT identity,source_sha,vector FROM am_vectors WHERE repository_id=?",
                [project_id],
            )?;
            assert_eq!(vectors.len(), 8);
            for candidate in candidates {
                let saved = vectors
                    .iter()
                    .find(|v| v["identity"] == candidate["identity"])
                    .unwrap();
                let input = format!("{}\n{}", s(&candidate, "title"), s(&candidate, "text"));
                let actual: Vec<f64> = serde_json::from_str(s(saved, "vector"))?;
                assert_eq!(actual, vector(&input));
                assert_eq!(saved["source_sha"], candidate["source_sha"]);
            }
            Ok(())
        })
        .unwrap();

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let expected_tokens = text::count("Shared note\nroute") + text::count("Shared note\nworker");
    println!(
        "G09 workflow original_passages=8 submitted_texts={} accounted_tokens={}",
        requests[0].len(),
        result["calls"][0]["input_tokens"]
    );
    assert_eq!(requests[0].len(), 2);
    assert_eq!(result["calls"][0]["input_tokens"], expected_tokens);
}
