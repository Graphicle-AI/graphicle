//! Caller query variants: the caller's alternative queries feed weighted
//! lexical and dense retrieval arms of `basic` answers and `recall`, with no
//! model call of their own. (Model-written variants, `query_expansion`, and
//! the `query_variants_gate` over the caller's are not supported:
//! the units `answer-query-expansion` and `retrieval-prf-depth-gate`.)
use axum::{Json, Router, extract::State, routing::post};
use graphicle_memory::text;
use graphicle_runtime::{
    jobs::Worker,
    model,
    store::{Store, s},
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

const QUESTION: &str = "What does shipping a release require?";
const NOTES: [(&str, &str); 3] = [
    (
        "release.md",
        "# Release\n\nShipping requires a signed release checklist and a green canary.\n",
    ),
    (
        "oncall.md",
        "# On-call\n\nThe pager rotation changes every Monday.\n",
    ),
    (
        "budget.md",
        "# Budget\n\nCloud spend is capped at forty thousand dollars a month.\n",
    ),
];
const REWRITE: &str = "requirements before shipping a release";
const HYPOTHETICAL: &str = "Shipping a release requires a signed checklist and a green canary.";

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
        let ws = store.create_workspace("Query variants").unwrap();
        let workspace = s(&ws, "id").to_string();
        let key = s(&ws, "capability").to_string();
        let repo = s(
            &store
                .register(
                    &workspace,
                    &key,
                    &json!({"project_key":"variants","name":"Variants"}),
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
        let files = NOTES;
        let state = f
            .store
            .project_status(&f.workspace, &f.key, &f.args(json!({})))
            .unwrap();
        let begin=f.store.sync_prepare(&f.workspace,&f.key,&f.args(json!({"expected_snapshot":state["repository"]["snapshot"],"manifest":files.iter().map(|(name,text)|json!({"path":name,"sha256":text::sha256(text.as_bytes()),"bytes":text.len()})).collect::<Vec<_>>()}))).unwrap();
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
    fn enqueue(&self, m: &Provider, args: Value) -> graphicle_runtime::Result<Value> {
        self.store
            .enqueue(&self.workspace, &self.key, &self.args(args), &m.config)
    }
    async fn run(&self, m: &Provider, args: Value) -> Value {
        let queued = self.enqueue(m, args).unwrap();
        Worker::new(self.store.clone(), m.config.clone())
            .unwrap()
            .once()
            .await
            .unwrap();
        self.store
            .job_status(
                &self.workspace,
                &self.key,
                &self.args(json!({"job_id":queued["id"]})),
            )
            .unwrap()
    }
}

#[derive(Clone)]
struct Seen {
    responses: Arc<Mutex<Vec<Value>>>,
    embeddings: Arc<Mutex<Vec<Vec<String>>>>,
}
struct Provider {
    config: model::Config,
    seen: Seen,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Provider {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Provider {
    fn responses(&self) -> Vec<Value> {
        self.seen.responses.lock().unwrap().clone()
    }
    fn embeddings(&self) -> Vec<Vec<String>> {
        self.seen.embeddings.lock().unwrap().clone()
    }
}

/// Answer requests cite the first source.
async fn inference(State(seen): State<Seen>, Json(request): Json<Value>) -> Json<Value> {
    let input: Value = serde_json::from_str(request["input"].as_str().unwrap()).unwrap();
    seen.responses
        .lock()
        .unwrap()
        .push(json!({"instructions":request["instructions"],"input":input}));
    let source = &input["sources"][0];
    let text = json!({"answer":"fast answer","abstain":false,
        "evidence":[{"id":source["id"],"quote":source["text"]}],"unresolved":[]})
    .to_string();
    Json(
        json!({"status":"completed","model":"fast-fixture","usage":{"input_tokens":150,"output_tokens":80},"output":[{"type":"message","content":[{"type":"output_text","text":text}]}]}),
    )
}
fn vector(input: &str) -> Vec<f64> {
    vec![
        input.len() as f64,
        input.bytes().map(u64::from).sum::<u64>() as f64,
    ]
}
async fn embeddings(State(seen): State<Seen>, Json(body): Json<Value>) -> Json<Value> {
    let inputs: Vec<String> = serde_json::from_value(body["input"].clone()).unwrap();
    seen.embeddings.lock().unwrap().push(inputs.clone());
    Json(json!({
        "data":inputs.iter().enumerate().map(|(index, input)|json!({
            "index":index,"embedding":vector(input)
        })).collect::<Vec<_>>(),
        "usage":{"prompt_tokens":inputs.iter().map(|input|text::count(input)).sum::<usize>()}
    }))
}
async fn provider() -> Provider {
    let seen = Seen {
        responses: Arc::new(Mutex::new(vec![])),
        embeddings: Arc::new(Mutex::new(vec![])),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/v1/responses", post(inference))
        .route("/v1/embeddings", post(embeddings))
        .with_state(seen.clone());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let base = format!("http://{address}/v1");
    let config = serde_json::from_value(json!({"default_model":"fast","embedding_model":"embed","models":{
        "fast":{"protocol":"responses","base_url":base,"model":"fast-fixture","input_limit":16000,"output_limit":4096},
        "embed":{"protocol":"chat","base_url":base,"model":"embed-fixture","input_limit":16000}
    }}))
    .unwrap();
    Provider { config, seen, task }
}

/// A synced and embedded project. Each scenario gets its own: query-vector and
/// reply caches would otherwise turn later calls into cached ones.
async fn embedded() -> (Fixture, Provider) {
    let f = Fixture::new();
    let m = provider().await;
    let done = f.run(&m, json!({"kind":"embed","max_calls":1})).await;
    assert_eq!(done["state"], "complete", "{done}");
    m.seen.embeddings.lock().unwrap().clear();
    (f, m)
}

/// Each reported variant's arm, kind, weight and dense arm (null when the
/// variant had no dense ranking).
fn reported(report: &Value) -> Vec<Value> {
    report
        .as_array()
        .unwrap_or_else(|| panic!("no query_variants report: {report}"))
        .iter()
        .map(|v| json!({"arm":v["arm"],"kind":v["kind"],"weight":v["weight"],"dense":v["dense"]}))
        .collect()
}
fn calls(done: &Value) -> usize {
    done["calls"].as_array().map_or(0, Vec::len)
}
fn texts(batches: &[&[&str]]) -> Vec<Vec<String>> {
    batches
        .iter()
        .map(|batch| batch.iter().map(|text| text.to_string()).collect())
        .collect()
}

#[tokio::test]
async fn variants_are_validated_when_queued() {
    let f = Fixture::new();
    let m = provider().await;
    let variant =
        |variants: Value| json!({"kind":"basic","question":QUESTION,"query_variants":variants});
    let nine: Vec<_> = (0..9)
        .map(|i| json!({"text":format!("release {i}")}))
        .collect();
    let cases = [
        (
            json!({"kind":"embed","query_variants":[{"text":"release"}]}),
            "query_variants apply only to basic and recall",
        ),
        (variant(json!({})), "query_variants must be an array"),
        (variant(json!(nine)), "At most 8 query variants"),
        (
            variant(json!(["release"])),
            "Query variants must be objects",
        ),
        (
            variant(json!([{"text":"release","source":"model"}])),
            "Query variant fields are text, weight and kind",
        ),
        (
            variant(json!([{"text":"   "}])),
            "Query variant text must be 1 to 4000 bytes",
        ),
        (
            variant(json!([{"text":"x".repeat(4001)}])),
            "Query variant text must be 1 to 4000 bytes",
        ),
        (
            variant(json!([{"text":"release","weight":5}])),
            "Query variant weights must be between 0 and 4",
        ),
        (
            variant(json!([{"text":"release","weight":"heavy"}])),
            "Query variant weights must be between 0 and 4",
        ),
        (
            variant(json!([{"text":"release","kind":"title"}])),
            "Query variant kind must be query, keyword or passage",
        ),
    ];
    for (args, message) in cases {
        let error = f.enqueue(&m, args.clone()).unwrap_err();
        assert_eq!(error.status.as_u16(), 400, "{args}: {}", error.message);
        assert_eq!(error.message, message, "{args}");
    }
    for args in [
        json!({"kind":"basic","question":QUESTION,
            "query_variants":[{"text":"release checklist","kind":"keyword","weight":2},
                {"text":HYPOTHETICAL,"kind":"passage"}]}),
        json!({"kind":"basic","question":QUESTION,"query_variants":null}),
        json!({"kind":"recall","question":QUESTION,"query_variants":[{"text":REWRITE}]}),
    ] {
        f.enqueue(&m, args.clone())
            .unwrap_or_else(|error| panic!("{args}: {}", error.message));
    }
}

#[tokio::test]
async fn caller_variants_fill_every_slot_of_a_basic_answer_in_order() {
    let (f, m) = embedded().await;
    let keywords = [
        "release",
        "checklist",
        "canary",
        "signed",
        "pager",
        "budget",
        "shipping",
    ];
    let mut variants: Vec<_> = keywords
        .iter()
        .map(|text| json!({"text":text,"kind":"keyword"}))
        .collect();
    variants.push(json!({"text":REWRITE,"weight":1.3}));
    let done = f
        .run(
            &m,
            json!({"kind":"basic","question":QUESTION,"query_variants":variants,"max_calls":4}),
        )
        .await;
    assert_eq!(done["state"], "complete", "{done}");
    let context = &done["result"]["context"];
    assert!(context.get("query_expansion").is_none(), "{context}");
    let reported = reported(&context["query_variants"]);
    assert_eq!(reported.len(), 8);
    for (i, variant) in reported[..7].iter().enumerate() {
        assert_eq!(
            variant,
            &json!({"arm":format!("lexical_q{}", i + 1),"kind":"keyword","weight":1.0,"dense":null})
        );
    }
    assert_eq!(
        reported[7],
        json!({"arm":"lexical_q8","kind":"query","weight":1.3,"dense":"semantic_q8"})
    );
    // One embedding call for the question and the query variant (keywords
    // have no dense arm), then the answer; no other model call.
    assert_eq!(m.embeddings(), texts(&[&[QUESTION, REWRITE]]));
    assert_eq!(m.responses().len(), 1);
    assert_eq!(calls(&done), 2, "{}", done["calls"]);
}

#[tokio::test]
async fn recall_fuses_caller_variants_without_a_chat_model() {
    let (f, m) = embedded().await;
    let done = f
        .run(
            &m,
            json!({"kind":"recall","question":QUESTION,"max_calls":4,"query_variants":[
                {"text":"release checklist","kind":"keyword"},
                {"text":REWRITE,"weight":1.5},
                {"text":HYPOTHETICAL,"kind":"passage"}]}),
        )
        .await;
    assert_eq!(done["state"], "complete", "{done}");
    let result = &done["result"];
    assert_eq!(
        reported(&result["query_variants"]),
        vec![
            json!({"arm":"lexical_q1","kind":"keyword","weight":1.0,"dense":null}),
            json!({"arm":"lexical_q2","kind":"query","weight":1.5,"dense":"semantic_q2"}),
            json!({"arm":"lexical_q3","kind":"passage","weight":1.0,"dense":"semantic_q3"}),
        ]
    );
    for arm in ["semantic", "semantic_q2", "semantic_q3"] {
        assert!(
            result["arms"].get(arm).is_some(),
            "{arm}: {}",
            result["arms"]
        );
    }
    assert!(result.get("query_expansion").is_none(), "{result}");
    assert_eq!(
        m.embeddings(),
        texts(&[&[QUESTION, REWRITE], &[HYPOTHETICAL]])
    );
    assert_eq!(result["model_calls"], 2, "{result}");
    assert!(m.responses().is_empty());
}
