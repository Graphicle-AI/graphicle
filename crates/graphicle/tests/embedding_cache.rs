//! Opt-in passage-vector cache: a second store holding the same passages reuses
//! the cached vectors instead of calling the provider again.
use axum::{Json, Router, extract::State, routing::post};
use graphicle_memory::text;
use graphicle_runtime::{
    jobs::Worker,
    model,
    store::{self, Store, s},
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

type Requests = Arc<Mutex<Vec<Vec<String>>>>;

fn vector(input: &str) -> Vec<f64> {
    vec![
        input.len() as f64,
        input.bytes().map(u64::from).sum::<u64>() as f64 + 0.25,
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

fn sorted(mut vectors: Vec<Vec<f64>>) -> Vec<Vec<f64>> {
    vectors.sort_by(|a, b| a.partial_cmp(b).unwrap());
    vectors
}

/// The vectors a store should hold for `contents`, each titled "Note".
fn expected(contents: &[&str]) -> Vec<Vec<f64>> {
    sorted(
        contents
            .iter()
            .map(|content| vector(&format!("Note\n{content}")))
            .collect(),
    )
}

/// Import `contents` into a fresh store, run one embed job, and return the
/// job result and the stored vectors in sorted order.
async fn embed(configuration: &model::Config, contents: &[&str]) -> (Value, Vec<Vec<f64>>) {
    let directory = tempfile::TempDir::new().unwrap();
    let store = Store::open(directory.path()).unwrap();
    let workspace = store.create_workspace("Embedding cache").unwrap();
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
    let documents: Vec<_> = contents
        .iter()
        .enumerate()
        .map(|(i, content)| json!({"id":format!("source-{i}"),"title":"Note","content":content}))
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
    let job = store
        .enqueue(
            ws,
            key,
            &json!({"repository_id":project_id,"kind":"embed","max_calls":4}),
            configuration,
        )
        .unwrap();
    assert!(
        Worker::new(store.clone(), configuration.clone())
            .unwrap()
            .once()
            .await
            .unwrap()
    );
    let result = store
        .job_status(
            ws,
            key,
            &json!({"repository_id":project_id,"job_id":job["id"]}),
        )
        .unwrap();
    assert_eq!(result["state"], "complete", "{result}");
    let vectors = store
        .authorized(ws, key, "read", |db, _| {
            store::rows(
                db,
                "SELECT vector FROM am_vectors WHERE repository_id=?",
                [project_id],
            )
        })
        .unwrap()
        .iter()
        .map(|row| serde_json::from_str(s(row, "vector")).unwrap())
        .collect();
    (result["result"].clone(), sorted(vectors))
}

#[tokio::test]
async fn a_second_store_reuses_cached_vectors_and_embeds_only_new_passages() {
    let cache_directory = tempfile::TempDir::new().unwrap();
    // This test binary holds a single test, so the variable cannot leak.
    unsafe {
        std::env::set_var(
            graphicle_runtime::embedding_cache::ENV,
            cache_directory.path().join("vectors.sqlite"),
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
    let configuration: model::Config = serde_json::from_value(json!({
        "default_model":"fixture","embedding_model":"fixture","models":{"fixture":{
            "protocol":"chat","base_url":format!("http://{address}/v1"),
            "model":"fixture","input_limit":1000
        }}
    }))
    .unwrap();

    let (first, first_vectors) = embed(&configuration, &["route", "worker", "queue"]).await;
    assert_eq!(first["embedded_passages"], 3, "{first}");
    assert_eq!(first["cached_passages"], 0, "{first}");
    assert_eq!(requests.lock().unwrap().len(), 1);

    let (second, second_vectors) = embed(&configuration, &["route", "worker", "queue"]).await;
    assert_eq!(second["embedded_passages"], 0, "{second}");
    assert_eq!(second["cached_passages"], 3, "{second}");
    assert_eq!(second["complete"], true, "{second}");
    assert_eq!(
        requests.lock().unwrap().len(),
        1,
        "a full hit calls nothing"
    );
    assert_eq!(first_vectors, expected(&["route", "worker", "queue"]));
    assert_eq!(second_vectors, first_vectors);

    let (third, third_vectors) = embed(&configuration, &["route", "changed", "queue"]).await;
    assert_eq!(third["embedded_passages"], 1, "{third}");
    assert_eq!(third["cached_passages"], 2, "{third}");
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1], vec!["Note\nchanged".to_owned()]);
    assert_eq!(third_vectors, expected(&["route", "changed", "queue"]));
}
