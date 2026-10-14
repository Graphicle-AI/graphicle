//! The grounded-v2 reader, the default `basic` answer contract: a question
//! naming what no source mentions is flagged for the answer model, a named
//! period is resolved by calendar, the contract is validated and pinned when
//! the job is queued, and reflect's source-only fallback reads with it.
use axum::{Json, Router, extract::State, routing::post};
use graphicle_runtime::{
    jobs::Worker,
    model,
    store::{Store, s},
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

const RELEASE: &str =
    "# Release\n\nShipping requires a signed release checklist and a green canary.\n";

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
        let ws = store.create_workspace("Grounded v2").unwrap();
        let workspace = s(&ws, "id").to_string();
        let key = s(&ws, "capability").to_string();
        let repo = s(
            &store
                .register(
                    &workspace,
                    &key,
                    &json!({"project_key":"grounded-v2","name":"Grounded v2"}),
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
        let files = [("release.md", RELEASE)];
        let state = f
            .store
            .project_status(&f.workspace, &f.key, &f.args(json!({})))
            .unwrap();
        let begin=f.store.sync_prepare(&f.workspace,&f.key,&f.args(json!({"expected_snapshot":state["repository"]["snapshot"],"manifest":files.iter().map(|(name,text)|json!({"path":name,"sha256":graphicle_memory::text::sha256(text.as_bytes()),"bytes":text.len()})).collect::<Vec<_>>()}))).unwrap();
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
    /// The payload the service stored for a queued job.
    fn payload(&self, queued: &Value) -> Value {
        let payload: String = self
            .store
            .connect()
            .unwrap()
            .query_row(
                "SELECT payload FROM am_jobs WHERE id=?",
                [s(queued, "id")],
                |r| r.get(0),
            )
            .unwrap();
        serde_json::from_str(&payload).unwrap()
    }
}

#[derive(Clone)]
struct Seen {
    answers: Arc<Mutex<Vec<Value>>>,
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

/// The answer model quotes the first source it was given.
async fn inference(State(seen): State<Seen>, Json(request): Json<Value>) -> Json<Value> {
    let input: Value = serde_json::from_str(request["input"].as_str().unwrap()).unwrap();
    seen.answers
        .lock()
        .unwrap()
        .push(json!({"instructions":request["instructions"],"input":input}));
    let source = &input["sources"][0];
    let answer = json!({"answer":"fast answer","abstain":false,
        "evidence":[{"id":source["id"],"quote":source["text"]}],"unresolved":[]});
    Json(
        json!({"status":"completed","model":"fast-fixture","usage":{"input_tokens":150,"output_tokens":80},"output":[{"type":"message","content":[{"type":"output_text","text":answer.to_string()}]}]}),
    )
}
async fn provider() -> Provider {
    let seen = Seen {
        answers: Arc::new(Mutex::new(vec![])),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/v1/responses", post(inference))
        .with_state(seen.clone());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let config = serde_json::from_value(json!({"default_model":"fast","models":{
        "fast":{"protocol":"responses","base_url":format!("http://{address}/v1"),"model":"fast-fixture","input_limit":16000,"output_limit":4096}
    }}))
    .unwrap();
    Provider { config, seen, task }
}

/// The first call (reflect's controller) gets text that is not JSON; every
/// later call is answered by [`inference`].
async fn controller_fails(State(seen): State<Seen>, Json(request): Json<Value>) -> Json<Value> {
    if seen.answers.lock().unwrap().is_empty() {
        seen.answers
            .lock()
            .unwrap()
            .push(json!({"instructions":request["instructions"],"input":Value::Null}));
        return Json(
            json!({"status":"completed","model":"fast-fixture","usage":{"input_tokens":150,"output_tokens":5},"output":[{"type":"message","content":[{"type":"output_text","text":"not json"}]}]}),
        );
    }
    inference(State(seen), Json(request)).await
}
async fn failing_controller() -> Provider {
    let seen = Seen {
        answers: Arc::new(Mutex::new(vec![])),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/v1/responses", post(controller_fails))
        .with_state(seen.clone());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let config = serde_json::from_value(json!({"default_model":"fast","models":{
        "fast":{"protocol":"responses","base_url":format!("http://{address}/v1"),"model":"fast-fixture","input_limit":16000,"output_limit":4096}
    }}))
    .unwrap();
    Provider { config, seen, task }
}

fn keys(input: &Value) -> Vec<String> {
    input.as_object().unwrap().keys().cloned().collect()
}

#[tokio::test]
async fn a_question_naming_what_no_source_mentions_is_flagged_for_the_answer_model() {
    let f = Fixture::new();
    let m = provider().await;
    let done = f
        .run(
            &m,
            json!({"kind":"basic","question":"What does shipping require at NovaTech?"}),
        )
        .await;
    assert_eq!(done["state"], "complete", "{done}");
    let result = &done["result"];
    assert_eq!(
        result["context"]["premise"],
        json!({"terms":["NovaTech"],"unmatched":["NovaTech"]})
    );
    assert_eq!(result["answer"], "fast answer");
    {
        let answers = m.seen.answers.lock().unwrap();
        assert_eq!(answers.len(), 1);
        let request = &answers[0];
        assert_eq!(request["input"]["unmatched_terms"], json!(["NovaTech"]));
        assert!(s(request, "instructions").contains("unmatched_terms lists"));
        assert_eq!(
            keys(&request["input"]),
            [
                "question",
                "question_time",
                "sources",
                "unmatched_terms",
                "final_check"
            ]
        );
    }
    // Terms the sources do mention add nothing; neither does the original contract.
    for (question, contract) in [
        ("What does the Release checklist require?", "grounded-v2"),
        ("What does shipping require at NovaTech?", "grounded-v1"),
    ] {
        let f = Fixture::new();
        let m = provider().await;
        let done = f
            .run(
                &m,
                json!({"kind":"basic","question":question,"answer_contract":contract}),
            )
            .await;
        assert_eq!(done["state"], "complete", "{question}: {done}");
        let answers = m.seen.answers.lock().unwrap();
        assert_eq!(answers.len(), 1, "{question}");
        assert!(answers[0]["input"].get("unmatched_terms").is_none());
        assert!(!s(&answers[0], "instructions").contains("unmatched_terms"));
    }
}

#[tokio::test]
async fn a_named_period_is_resolved_by_calendar_for_the_answer_model() {
    let f = Fixture::new();
    let m = provider().await;
    let done = f
        .run(
            &m,
            json!({"kind":"basic","question":"What did shipping require last week?","question_time":"2026-06-10T12:00:00Z"}),
        )
        .await;
    assert_eq!(done["state"], "complete", "{done}");
    let period = json!({"start":"2026-06-01","end":"2026-06-07","rule":"last_unit"});
    assert_eq!(done["result"]["context"]["question_period"], period);
    let answers = m.seen.answers.lock().unwrap();
    let request = &answers[0];
    assert_eq!(request["input"]["question_period"], period);
    assert!(s(request, "instructions").contains("question_period is the calendar period"));
    assert_eq!(
        keys(&request["input"]),
        [
            "question",
            "question_time",
            "sources",
            "question_period",
            "final_check"
        ]
    );
}

#[tokio::test]
async fn answer_contracts_are_validated_and_pinned_when_queued() {
    let f = Fixture::new();
    let m = provider().await;
    for (args, error) in [
        (
            json!({"kind":"basic","question":"x","answer_contract":"grounded-v9"}),
            "Unknown answer_contract",
        ),
        (
            json!({"kind":"basic","question":"x","answer_contract":7}),
            "Unknown answer_contract",
        ),
        (
            json!({"kind":"reflect","question":"x","answer_contract":"grounded-v2"}),
            "answer_contract applies only to basic question answering",
        ),
        (
            json!({"kind":"recall","question":"x","answer_contract":"grounded-v3"}),
            "answer_contract applies only to basic question answering",
        ),
    ] {
        let failed = f.enqueue(&m, args.clone()).unwrap_err();
        assert_eq!(failed.status.as_u16(), 400, "{args}: {}", failed.message);
        assert!(failed.message.contains(error), "{args}: {}", failed.message);
    }
    // A basic job without a contract is pinned to the default, grounded-v2;
    // a named contract is kept.
    for (args, contract) in [
        (json!({"kind":"basic","question":"q"}), "grounded-v2"),
        (
            json!({"kind":"basic","question":"q2","answer_contract":"grounded-v1"}),
            "grounded-v1",
        ),
        (
            json!({"kind":"basic","question":"q3","answer_contract":"grounded-v3","observer_notes":false}),
            "grounded-v3",
        ),
    ] {
        let queued = f.enqueue(&m, args.clone()).unwrap();
        assert_eq!(f.payload(&queued)["answer_contract"], contract, "{args}");
    }
    // Other kinds carry no contract.
    let queued = f
        .enqueue(&m, json!({"kind":"reflect","question":"q"}))
        .unwrap();
    assert!(f.payload(&queued).get("answer_contract").is_none());
}

/// When reflect's controller reply breaks its output contract, the
/// source-only answer reflect falls back to reads with grounded-v2, the
/// `basic` default (it used the original grounded-v1 prompt).
#[tokio::test]
async fn reflects_source_only_fallback_reads_with_grounded_v2() {
    let f = Fixture::new();
    let m = failing_controller().await;
    let done = f
        .run(
            &m,
            json!({"kind":"reflect","question":"What does shipping require at NovaTech?","verify_answer":false,"max_calls":3}),
        )
        .await;
    assert_eq!(done["state"], "complete", "{done}");
    let result = &done["result"];
    assert_eq!(
        result["trace"][0]["fallback"], "source_only_answer",
        "{result}"
    );
    assert_eq!(result["answer"], "fast answer");
    let answers = m.seen.answers.lock().unwrap();
    assert_eq!(answers.len(), 2, "the controller, then the fallback reader");
    let reader = &answers[1];
    assert!(
        s(reader, "instructions").contains("The input ends with final_check"),
        "{reader}"
    );
    assert_eq!(reader["input"]["unmatched_terms"], json!(["NovaTech"]));
    assert_eq!(
        keys(&reader["input"]),
        [
            "question",
            "question_time",
            "sources",
            "unmatched_terms",
            "final_check"
        ]
    );
}
