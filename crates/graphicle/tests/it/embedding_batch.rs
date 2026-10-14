//! G09: qualify a small MemOS-derived optimization at the real HTTP boundary.
//! These fixtures measure request work and transport correctness, not model quality.
use axum::{Json, Router, extract::State, routing::post};
use graphicle_memory::text;
use graphicle_runtime::{model, model::Profile};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

const PROTOCOLS: &[&str] = &[
    "chat",
    "responses",
    "azure_chat",
    "azure_responses",
    "ollama",
    "gemini",
];

struct Endpoint {
    protocol: String,
    reported: bool,
    malformed: bool,
    requests: Mutex<Vec<Vec<String>>>,
}

fn vector(input: &str) -> Vec<f64> {
    vec![
        input.len() as f64,
        input.bytes().map(u64::from).sum::<u64>() as f64,
    ]
}

async fn handle(State(state): State<Arc<Endpoint>>, Json(body): Json<Value>) -> Json<Value> {
    let texts: Vec<String> = if state.protocol == "gemini" {
        body["requests"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                r["content"]["parts"][0]["text"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect()
    } else {
        serde_json::from_value(body["input"].clone()).unwrap()
    };
    state.requests.lock().unwrap().push(texts.clone());
    let mut vectors: Vec<_> = texts.iter().map(|t| vector(t)).collect();
    if state.malformed {
        vectors.pop();
    }
    let mut result = match state.protocol.as_str() {
        "gemini" => {
            json!({"embeddings":vectors.into_iter().map(|v|json!({"values":v})).collect::<Vec<_>>()})
        }
        "ollama" => json!({"embeddings":vectors}),
        // Deliberately return indexed embeddings out of order.
        _ => json!({"data":vectors.into_iter().enumerate().rev()
            .map(|(i,v)|json!({"index":i,"embedding":v})).collect::<Vec<_>>()}),
    };
    if state.reported {
        let tokens = texts.iter().map(|s| text::count(s)).sum::<usize>();
        match state.protocol.as_str() {
            "ollama" => result["prompt_eval_count"] = json!(tokens),
            // batchEmbedContents returns embeddings without token usage.
            "gemini" => {}
            _ => result["usage"] = json!({"prompt_tokens":tokens}),
        }
    }
    Json(result)
}

struct Mock {
    state: Arc<Endpoint>,
    profile: Profile,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl Mock {
    async fn new(protocol: &str, reported: bool, malformed: bool) -> Self {
        let state = Arc::new(Endpoint {
            protocol: protocol.into(),
            reported,
            malformed,
            requests: Mutex::new(vec![]),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/{*path}", post(handle))
            .with_state(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let profile: Profile =
            serde_json::from_value(json!({"protocol":protocol,"model":"fixture",
            "base_url":format!("http://{address}"),"input_limit":1000}))
            .unwrap();
        let config: model::Config = serde_json::from_value(json!({
            "default_model":"fixture","models":{"fixture":profile}
        }))
        .unwrap();
        config.validate().unwrap();
        Self {
            state,
            profile,
            server,
        }
    }
}

fn input() -> Vec<String> {
    [
        "route", "worker", "route", "worker", "route", "worker", "route", "worker",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

#[tokio::test]
async fn exact_duplicate_inputs_are_sent_once_and_restored_in_order() {
    let client = model::client().unwrap();
    let mut failures = Vec::new();
    for protocol in PROTOCOLS {
        let mock = Mock::new(protocol, true, false).await;
        let texts = input();
        let (vectors, tokens, reported) =
            model::embed_with_credential(&client, &mock.profile, &texts, None)
                .await
                .unwrap();
        assert_eq!(vectors, texts.iter().map(|t| vector(t)).collect::<Vec<_>>());
        let requests = mock.state.requests.lock().unwrap();
        println!(
            "G09 protocol={protocol} original_texts={} submitted_texts={} accounted_tokens={tokens}",
            texts.len(),
            requests[0].len()
        );
        if requests.as_slice() != [vec!["route".to_owned(), "worker".to_owned()]]
            || tokens != text::count("route") + text::count("worker")
        {
            failures.push(*protocol);
        }
        assert_eq!(reported, *protocol != "gemini", "{protocol}");
    }
    assert!(failures.is_empty(), "Repeated inputs reached: {failures:?}");
}

#[tokio::test]
async fn unreported_usage_estimates_only_submitted_texts() {
    let client = model::client().unwrap();
    let mut failures = Vec::new();
    for protocol in PROTOCOLS {
        let mock = Mock::new(protocol, false, false).await;
        let (_, tokens, reported) =
            model::embed_with_credential(&client, &mock.profile, &input(), None)
                .await
                .unwrap();
        if tokens != text::count("route") + text::count("worker") {
            failures.push(*protocol);
        }
        assert!(!reported);
    }
    assert!(
        failures.is_empty(),
        "Original input usage estimated: {failures:?}"
    );
}

#[tokio::test]
async fn normalization_and_cross_request_reuse_are_not_introduced() {
    let client = model::client().unwrap();
    let mock = Mock::new("chat", true, false).await;
    let texts: Vec<_> = ["Route", "route", "route ", "résumé", "resume"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    for suffix in ["first", "second"] {
        let mut profile = mock.profile.clone();
        profile.model = suffix.into();
        let (vectors, _, _) = model::embed_with_credential(&client, &profile, &texts, None)
            .await
            .unwrap();
        assert_eq!(vectors, texts.iter().map(|t| vector(t)).collect::<Vec<_>>());
    }
    assert_eq!(
        mock.state.requests.lock().unwrap().as_slice(),
        &[texts.clone(), texts]
    );
}

#[tokio::test]
async fn malformed_provider_shapes_remain_errors() {
    let client = model::client().unwrap();
    for protocol in PROTOCOLS {
        let mock = Mock::new(protocol, true, true).await;
        assert!(
            model::embed_with_credential(&client, &mock.profile, &input(), None)
                .await
                .is_err(),
            "{protocol}"
        );
    }
}

#[tokio::test]
async fn duplicate_inputs_cannot_bypass_original_request_limits() {
    let client = model::client().unwrap();
    let mock = Mock::new("chat", true, false).await;
    for texts in [
        vec![],
        vec!["route".to_owned(); 33],
        vec!["x".repeat(32001)],
    ] {
        assert!(
            model::embed_with_credential(&client, &mock.profile, &texts, None)
                .await
                .is_err()
        );
    }
    let mut profile = mock.profile.clone();
    profile.input_limit = 1;
    assert!(
        model::embed_with_credential(&client, &profile, &input(), None)
            .await
            .is_err()
    );
    assert!(mock.state.requests.lock().unwrap().is_empty());
}
