use axum::{Json, Router, extract::State, routing::post};
use graphicle_runtime::{
    api, claims,
    jobs::Worker,
    local, memory, model,
    store::{Store, s},
};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tempfile::TempDir;

struct Fixture {
    dir: TempDir,
    store: Store,
    workspace: String,
    key: String,
    project: String,
}

#[tokio::test]
async fn native_answering_combines_projected_graph_and_memory_and_honors_switches() {
    for kind in ["basic", "reflect"] {
        for graph in [true, false] {
            for memory in [true, false] {
                let f = Fixture::new();
                f.sync(&[
                    ("a.py", "def alpha():\n    return 7\n"),
                    (
                        "b.py",
                        "from a import alpha\n\ndef run():\n    return alpha()\n",
                    ),
                ]);
                f.store
                    .graph_project(&f.workspace, &f.key, &f.project)
                    .unwrap();
                f.store.note(&f.workspace, &f.key, &f.args(json!({"title":"alpha return value",
                    "body":"alpha returns seven", "evidence":[{"id":"file:a.py","quote":"return 7"}]}))).unwrap();
                let mut answer = json!({"answer":"alpha returns seven","abstain":false,
                    "evidence":[{"id":"file:a.py","quote":"return 7"}],"unresolved":[]});
                if kind == "reflect" {
                    answer.as_object_mut().unwrap().remove("abstain");
                    answer["action"] = json!("answer");
                    answer["query"] = Value::Null;
                    answer["queries"] = json!([]);
                    answer["reads"] = json!([]);
                    answer["source_id"] = Value::Null;
                    answer["start_byte"] = Value::Null;
                }
                let remote = mock(vec![answer], None).await;
                let result = run(
                    &f,
                    &remote,
                    json!({"kind":kind,"question":"alpha",
                    "max_calls":1,"graph_retrieval":graph,"memory_retrieval":memory}),
                )
                .await;
                assert_eq!(result["state"], "complete", "{result}");
                let context = &result["result"]["context"];
                assert_eq!(
                    context["mechanisms"]["code_graph_searched"], graph,
                    "{context}"
                );
                assert_eq!(
                    context["mechanisms"]["reviewed_memory_included"],
                    usize::from(memory),
                    "{context}"
                );
                let diagnostics = if kind == "reflect" {
                    &context["retrievals"][0]
                } else {
                    context
                };
                assert_eq!(
                    diagnostics["code_graph"]["status"],
                    if graph { "fused" } else { "disabled" }
                );
            }
        }
    }
}

#[tokio::test]
async fn basic_keeps_explicit_retrieval_controls() {
    let f = Fixture::new();
    let quote = "Shipping requires the access check.";
    f.sync(&[("rules.md", quote)]);
    let remote = mock(vec![json!({"answer":quote,"abstain":false,"evidence":[{"id":"file:rules.md","quote":quote}],"unresolved":[]})], None).await;
    let result = run(
        &f,
        &remote,
        json!({"kind":"basic","question":"What does shipping require?",
        "max_calls":1,"stem":false,"speaker":false,"pack_focus":false,
        "arm_weights":{"lexical":2.0},"rrf_k":30,"compact_citations":true}),
    )
    .await;
    assert_eq!(result["state"], "complete", "{result}");
    let packet = &result["result"]["context"];
    assert_eq!(packet["retrieval_defaults"], json!({}), "{packet}");
    assert_eq!(packet["arm_weights"], json!({"lexical":2.0}), "{packet}");
}

#[tokio::test]
async fn answer_jobs_reject_unknown_local_retrieval_profiles_before_queueing() {
    let f = Fixture::new();
    f.sync(&[("rules.md", "Shipping requires the access check.")]);
    let remote = mock(vec![], None).await;
    let result = f.store.enqueue(
        &f.workspace,
        &f.key,
        &f.args(json!({"kind":"basic",
        "question":"What does shipping require?","local_reranker_profile":"missing"})),
        &remote.config,
    );
    assert!(
        result.is_err(),
        "A requested reranker must not be silently ignored"
    );
    assert_eq!(remote.state.count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn basic_can_combine_observer_reading_with_a_reserved_source_review() {
    let f = Fixture::new();
    let quote = "Shipping requires both the access check and the restore check.";
    f.sync(&[("rules.md", quote)]);
    let evidence = json!([{"id":"file:rules.md","quote":quote}]);
    let remote=mock(vec![
        json!({"answer":"Run the access check.","abstain":false,"evidence":evidence,"unresolved":[]}),
        json!({"reviewed_answer":{"answer":"Run both the access and restore checks.","abstain":false,"evidence":evidence,"unresolved":[]},
            "coverage":[{"requested_part":"Required checks","status":"answered","evidence":evidence}]})],None).await;
    let result = run(
        &f,
        &remote,
        json!({"kind":"basic","question":"Which checks are required?",
        "observer_notes":true,"verify_answer":true,"max_calls":2}),
    )
    .await;
    assert_eq!(result["state"], "complete", "{result}");
    assert_eq!(result["result"]["review"]["status"], "completed");
    assert_eq!(
        result["result"]["answer"],
        "Run both the access and restore checks."
    );
    assert_eq!(remote.state.count.load(Ordering::SeqCst), 2);
    assert!(
        remote.state.requests.lock().unwrap()[1]
            .to_string()
            .contains("ORIGINAL sources")
    );
}

#[tokio::test]
async fn ordinary_questions_read_supporting_originals_and_expose_memory_controls() {
    let f = Fixture::new();
    let quote = "For a Sev-1, page Mira via channel 42.";
    f.sync(&[("routing.txt", quote)]);
    f.store.note(&f.workspace,&f.key,&f.args(json!({"title":"Emergency escalation owner","body":"Emergency escalation owner is Mira.","evidence":[{"id":"file:routing.txt","quote":quote}]}))).unwrap();
    let agent_context = f
        .store
        .project_context(
            &f.workspace,
            &f.key,
            &f.args(json!({"question":"emergency escalation owner","max_tokens":1000})),
        )
        .unwrap();
    assert_eq!(agent_context["mechanisms"]["graph_searched"], true);
    assert!(
        agent_context["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["text"].as_str().unwrap().contains(quote))
    );
    let no_memory = f.store.project_context(&f.workspace,&f.key,&f.args(json!({"question":"emergency escalation owner","memory_retrieval":false,"graph_retrieval":false}))).unwrap();
    assert!(no_memory["evidence"].as_array().unwrap().is_empty());
    assert_eq!(no_memory["mechanisms"]["graph_searched"], false);
    let answer = json!({"action":"answer","query":null,"queries":[],"source_id":null,"start_byte":null,"reads":[],
        "answer":"Page Mira via channel 42.","evidence":[{"id":"file:routing.txt","quote":quote}],"unresolved":[]});
    let abstain = json!({"action":"abstain","query":null,"queries":[],"source_id":null,"start_byte":null,"reads":[],"answer":"The available sources do not say.","evidence":[],"unresolved":["Escalation contact"]});
    let remote = mock(vec![answer, abstain], None).await;
    let enabled = run(&f,&remote,json!({"kind":"reflect","question":"emergency escalation owner","max_calls":1,"context_tokens":1000})).await;
    assert_eq!(enabled["state"], "complete", "{enabled}");
    let context = &enabled["result"]["context"];
    assert_eq!(context["mechanisms"]["reviewed_memory_included"], 1);
    assert_eq!(context["mechanisms"]["graph_searched"], true);
    assert_eq!(
        context["memory_support"]["memories"][0]["all_cited_quotes_present"],
        true
    );
    assert!(
        remote.state.requests.lock().unwrap()[0]
            .to_string()
            .contains(quote)
    );
    let disabled = run(&f,&remote,json!({"kind":"reflect","question":"emergency escalation owner","max_calls":1,"context_tokens":1000,"memory_retrieval":false,"graph_retrieval":false})).await;
    assert_eq!(disabled["state"], "complete", "{disabled}");
    assert!(
        disabled["result"]["context"]["evidence"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        disabled["result"]["context"]["mechanisms"]["graph_searched"],
        false
    );
    assert_eq!(remote.state.count.load(Ordering::SeqCst), 2);
    assert!(
        f.store
            .enqueue(
                &f.workspace,
                &f.key,
                &f.args(json!({"kind":"reflect","question":"owner","graph_retrieval":"false"})),
                &remote.config
            )
            .is_err()
    );
}

#[tokio::test]
async fn basic_bounds_large_model_defaults_to_the_evidence_packers_capacity() {
    let f = Fixture::new();
    f.sync(&[("rules.md", "Shipping requires the access check.")]);
    let mut remote = mock(vec![json!({"answer":"The access check is required.","abstain":false,"evidence":[{"id":"file:rules.md","quote":"Shipping requires the access check."}],"unresolved":[]})], None).await;
    remote.config.models.get_mut("fixture").unwrap().input_limit = 128000;
    let result = run(
        &f,
        &remote,
        json!({"kind":"basic","question":"What does shipping require?","max_calls":1}),
    )
    .await;
    assert_eq!(result["state"], "complete", "{result}");
    assert_eq!(result["result"]["context"]["max_evidence_tokens"], 16000);
}
/// Installs the edition under test before a fixture is built: nothing in this
/// Core test binary. An extension crate can re-run this suite from its own
/// test binary, whose root `edition_setup` installs its extensions (`crate::`
/// names the root of whichever test binary the suite is compiled into).
#[allow(dead_code)]
fn edition_setup() {}

impl Fixture {
    fn new() -> Self {
        crate::edition_setup();
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let ws = store.create_workspace("Development fixture").unwrap();
        let workspace = s(&ws, "id").to_string();
        let key = s(&ws, "capability").to_string();
        let p = store
            .register(
                &workspace,
                &key,
                &json!({"project_key":"fixture","name":"Workshop"}),
            )
            .unwrap();
        let project = s(&p, "id").to_string();
        Self {
            dir,
            store,
            workspace,
            key,
            project,
        }
    }
    fn args(&self, mut args: Value) -> Value {
        args["repository_id"] = json!(self.project);
        args
    }
    fn sync(&self, files: &[(&str, &str)]) {
        let status = self
            .store
            .project_status(&self.workspace, &self.key, &self.args(json!({})))
            .unwrap();
        let manifest:Vec<_>=files.iter().map(|(name,text)|json!({"path":name,"sha256":graphicle_memory::text::sha256(text.as_bytes()),"bytes":text.len()})).collect();
        let prepare=self.store.sync_prepare(&self.workspace,&self.key,&self.args(json!({"manifest":manifest,"expected_snapshot":status["repository"]["snapshot"]}))).unwrap();
        if prepare["replayed"] == true {
            return;
        }
        if !files.is_empty() {
            self.store.sync_upload(&self.workspace,&self.key,&self.args(json!({"sync_id":prepare["sync_id"],"files":files.iter().map(|(path,content)|json!({"path":path,"content":content})).collect::<Vec<_>>()}))).unwrap();
        }
        self.store
            .sync_commit(
                &self.workspace,
                &self.key,
                &self.args(json!({"sync_id":prepare["sync_id"]})),
            )
            .unwrap();
    }
    fn memories(&self) -> Vec<Value> {
        self.store
            .project_read(
                &self.workspace,
                &self.key,
                "memories",
                &self.args(json!({"include_retired":true})),
            )
            .unwrap()
            .as_array()
            .unwrap()
            .clone()
    }
    fn approve(&self, memory: &Value) -> Value {
        self.store.review_memory(&self.workspace,&self.key,&self.args(json!({"memory_id":memory["id"],"expected_revision":memory["revision"],"decision":"approve","maintainer":"Fixture reviewer","reason":"Checked the cited original text"}))).unwrap()
    }
    fn status(&self, id: &str) -> Value {
        self.store
            .job_status(&self.workspace, &self.key, &self.args(json!({"job_id":id})))
            .unwrap()
    }
}
#[derive(Clone)]
struct ModelState {
    responses: Arc<Mutex<Vec<Value>>>,
    requests: Arc<Mutex<Vec<Value>>>,
    count: Arc<AtomicUsize>,
    gate: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
}
struct Mock {
    config: model::Config,
    state: ModelState,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn provider(State(state): State<ModelState>, Json(request): Json<Value>) -> Json<Value> {
    state.count.fetch_add(1, Ordering::SeqCst);
    state.requests.lock().unwrap().push(request);
    if let Some((entered, release)) = &state.gate {
        entered.notify_one();
        release.notified().await;
    }
    let value = state.responses.lock().unwrap().remove(0);
    Json(
        if value.get("status").is_some() || value.get("choices").is_some() {
            value
        } else {
            json!({"status":"completed","model":"fixture-model","usage":{"input_tokens":150,"output_tokens":80},"output":[{"type":"message","content":[{"type":"output_text","text":value.to_string()}]}]})
        },
    )
}
async fn embedding_provider(
    State(state): State<ModelState>,
    Json(request): Json<Value>,
) -> Json<Value> {
    state.count.fetch_add(1, Ordering::SeqCst);
    state.requests.lock().unwrap().push(request.clone());
    Json(
        json!({"data":request["input"].as_array().unwrap().iter().enumerate().map(|(i,text)|json!({"index":i,"embedding":if s(&json!({"value":text}),"value").contains("loud"){vec![0.0,1.0]}else{vec![1.0,0.0]}})).collect::<Vec<_>>(),"usage":{"prompt_tokens":25}}),
    )
}
async fn mock(
    responses: Vec<Value>,
    gate: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
) -> Mock {
    let state = ModelState {
        responses: Arc::new(Mutex::new(responses)),
        requests: Arc::new(Mutex::new(vec![])),
        count: Arc::new(AtomicUsize::new(0)),
        gate,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/v1/responses", post(provider))
        .route("/v1/chat/completions", post(provider))
        .route("/v1/embeddings", post(embedding_provider))
        .with_state(state.clone());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let config=serde_json::from_value(json!({"default_model":"fixture","models":{"fixture":{"protocol":"responses","base_url":format!("http://{address}/v1"),"model":"fixture-model","input_limit":16000,"output_limit":2048}}})).unwrap();
    Mock {
        config,
        state,
        task,
    }
}
fn claim(statement: &str, action: &str, target: Option<&str>) -> Value {
    json!({"statement":statement,"subject":"Mira","relation":"prefers","object":"quiet rooms","speaker":"user","modality":"preference","conditions":[],"action":action,"target_id":target,
        "valid_at":null,"invalid_at":null,"time_basis":"unknown","anchor_id":null,"evidence":[{"id":"file:notes.txt","quote":statement}]})
}
async fn run(f: &Fixture, mock: &Mock, args: Value) -> Value {
    let job = f
        .store
        .enqueue(&f.workspace, &f.key, &f.args(args), &mock.config)
        .unwrap();
    let worker = Worker::new(f.store.clone(), mock.config.clone()).unwrap();
    assert!(worker.once().await.unwrap());
    f.status(s(&job, "id"))
}

#[test]
fn original_sources_survive_restart_and_reject_cross_workspace_access() {
    let f = Fixture::new();
    f.sync(&[
        (
            "notes.txt",
            "Mira prefers quiet rooms.\nKeep the entrance clear.",
        ),
        ("main.py", "from util import read\nread()\n"),
        ("util.py", "def read():\n    return 7\n"),
    ]);
    let packet = f
        .store
        .project_context(
            &f.workspace,
            &f.key,
            &f.args(json!({"question":"What rooms does Mira prefer?"})),
        )
        .unwrap();
    assert!(s(&packet, "markdown").contains("quiet rooms"));
    assert!(packet["tokens"].as_u64().unwrap() <= 3000);
    let other = f.store.create_workspace("Other").unwrap();
    assert!(
        f.store
            .project_context(
                s(&other, "id"),
                s(&other, "capability"),
                &f.args(json!({"question":"Mira"}))
            )
            .is_err()
    );
    let reopened = Store::open(f.dir.path()).unwrap();
    let read = reopened
        .read_project_source(&f.workspace, &f.key, &f.args(json!({"path":"notes.txt"})))
        .unwrap();
    assert_eq!(
        read["quote"],
        "Mira prefers quiet rooms.\nKeep the entrance clear."
    );
    // The file graph survives the restart too: main.py still imports util.py.
    let status = reopened
        .project_status(&f.workspace, &f.key, &f.args(json!({})))
        .unwrap();
    assert!(
        status["graph"]["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|edge| edge["source"] == "main.py" && edge["target"] == "util.py"),
        "{}",
        status["graph"]
    );
}
#[test]
fn failed_source_upload_and_racing_snapshots_leave_the_previous_view() {
    let f = Fixture::new();
    f.sync(&[("a.txt", "old")]);
    let before = f
        .store
        .project_status(&f.workspace, &f.key, &f.args(json!({})))
        .unwrap()["repository"]["snapshot"]
        .clone();
    let prepare=f.store.sync_prepare(&f.workspace,&f.key,&f.args(json!({"expected_snapshot":before,"manifest":[{"path":"a.txt","sha256":graphicle_memory::text::sha256(b"new"),"bytes":3}]}))).unwrap();
    assert!(
        f.store
            .sync_upload(
                &f.workspace,
                &f.key,
                &f.args(
                    json!({"sync_id":prepare["sync_id"],"files":[{"path":"a.txt","content":"bad"}]})
                )
            )
            .is_err()
    );
    assert!(
        f.store
            .sync_commit(
                &f.workspace,
                &f.key,
                &f.args(json!({"sync_id":prepare["sync_id"]}))
            )
            .is_err()
    );
    assert_eq!(
        f.store
            .read_project_source(&f.workspace, &f.key, &f.args(json!({"path":"a.txt"})))
            .unwrap()["quote"],
        "old"
    );
    f.sync(&[("a.txt", "latest")]);
    assert!(
        f.store
            .sync_prepare(
                &f.workspace,
                &f.key,
                &f.args(json!({"expected_snapshot":before,"manifest":[]}))
            )
            .is_err()
    );
}
#[tokio::test]
async fn extraction_runs_a_model_then_reuses_completed_pages_and_requires_review() {
    let f = Fixture::new();
    let text = "Mira prefers quiet rooms.";
    f.sync(&[("notes.txt", text)]);
    let remote = mock(vec![json!({"claims":[claim(text,"ADD",None)]})], None).await;
    let job = run(&f, &remote, json!({"kind":"extract"})).await;
    assert_eq!(job["state"], "complete", "{job}");
    assert_eq!(job["result"]["drafts"].as_array().unwrap().len(), 1);
    assert_eq!(f.memories()[0]["state"], "draft");
    let replay = run(&f, &remote, json!({"kind":"extract"})).await;
    assert_eq!(replay["state"], "complete", "{replay}");
    assert_eq!(replay["result"]["reused_chunks"], 1);
    assert_eq!(remote.state.count.load(Ordering::SeqCst), 1);
    let packet = f
        .store
        .project_context(
            &f.workspace,
            &f.key,
            &f.args(json!({"question":"Mira rooms"})),
        )
        .unwrap();
    assert!(packet["memories"].as_array().unwrap().is_empty());
    f.approve(&f.memories()[0]);
    assert_eq!(f.memories()[0]["state"], "reviewed");
    // Repeat before changing prior reviewed context: page coverage and calls are explicit.
    assert_eq!(remote.state.count.load(Ordering::SeqCst), 1);
    let request = &remote.state.requests.lock().unwrap()[0];
    assert_eq!(request["store"], false);
    assert_eq!(request["text"]["format"]["strict"], true);
    let bad = f.store.enqueue(
        &f.workspace,
        &f.key,
        &f.args(json!({"kind":"extract"})),
        &model::Config::default(),
    );
    assert!(bad.is_err());
}
#[tokio::test]
async fn absent_quotes_and_incomplete_responses_never_become_memory() {
    let f = Fixture::new();
    f.sync(&[("notes.txt", "Mira prefers quiet rooms.")]);
    let remote = mock(
        vec![
            json!({"claims":[claim("Mira booked the hotel.","ADD",None)]}),
            json!({"status":"incomplete","output":[]}),
        ],
        None,
    )
    .await;
    let bad = run(&f, &remote, json!({"kind":"extract"})).await;
    assert_eq!(bad["state"], "failed");
    assert!(f.memories().is_empty());
    let incomplete = run(
        &f,
        &remote,
        json!({"kind":"extract","request_id":"incomplete"}),
    )
    .await;
    assert_eq!(incomplete["state"], "failed");
    assert!(f.memories().is_empty());
    assert_eq!(incomplete["calls"][0]["call_state"], "unknown_usage");
}
#[tokio::test]
async fn revocation_while_the_model_runs_prevents_commit() {
    let f = Fixture::new();
    f.sync(&[("notes.txt", "Mira prefers quiet rooms.")]);
    let granted = f
        .store
        .grant(
            &f.workspace,
            &f.key,
            &json!({"role":"agent","label":"Development agent"}),
        )
        .unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let remote = mock(
        vec![json!({"claims":[claim("Mira prefers quiet rooms.","ADD",None)]})],
        Some((entered.clone(), release.clone())),
    )
    .await;
    let queued = f
        .store
        .enqueue(
            &f.workspace,
            s(&granted, "capability"),
            &f.args(json!({"kind":"extract"})),
            &remote.config,
        )
        .unwrap();
    let worker = Worker::new(f.store.clone(), remote.config.clone()).unwrap();
    let running = tokio::spawn(async move { worker.once().await });
    entered.notified().await;
    f.store
        .revoke(&f.workspace, &f.key, s(&granted, "id"))
        .unwrap();
    release.notify_one();
    running.await.unwrap().unwrap();
    assert!(f.memories().is_empty());
    assert_eq!(f.status(s(&queued, "id"))["state"], "cancelled");
}
#[tokio::test]
async fn changed_source_while_the_model_runs_prevents_commit() {
    let f = Fixture::new();
    f.sync(&[("notes.txt", "Mira prefers quiet rooms.")]);
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let remote = mock(
        vec![json!({"claims":[claim("Mira prefers quiet rooms.","ADD",None)]})],
        Some((entered.clone(), release.clone())),
    )
    .await;
    let queued = f
        .store
        .enqueue(
            &f.workspace,
            &f.key,
            &f.args(json!({"kind":"extract"})),
            &remote.config,
        )
        .unwrap();
    let worker = Worker::new(f.store.clone(), remote.config.clone()).unwrap();
    let running = tokio::spawn(async move { worker.once().await });
    entered.notified().await;
    f.sync(&[("notes.txt", "Mira no longer prefers quiet rooms.")]);
    release.notify_one();
    running.await.unwrap().unwrap();
    assert!(f.memories().is_empty());
    assert_eq!(f.status(s(&queued, "id"))["state"], "failed");
}
#[tokio::test]
async fn reflection_reads_later_evidence_and_cites_it_before_retaining() {
    let f = Fixture::new();
    f.sync(&[
        ("notes.txt", "Mira prefers quiet rooms."),
        ("decision.txt", "The event has moved to Friday."),
    ]);
    let search = json!({"action":"read","queries":[],"reads":[],"query":null,"source_id":"file:decision.txt","start_byte":0,"answer":"","evidence":[],"unresolved":[]});
    let answer = json!({"action":"answer","queries":[],"reads":[],"query":null,"source_id":null,"start_byte":null,"answer":"The event is on Friday.","evidence":[{"id":"file:decision.txt","quote":"The event has moved to Friday."}],"unresolved":[]});
    let remote = mock(vec![search, answer], None).await;
    let result = run(
        &f,
        &remote,
        json!({"kind":"reflect","question":"When is the event?","retain":true,"max_calls":2,"verify_answer":false}),
    )
    .await;
    assert_eq!(result["state"], "complete", "{result}");
    assert_eq!(result["result"]["steps"], 2);
    assert_eq!(f.memories()[0]["kind"], "reflection");
    assert_eq!(f.memories()[0]["state"], "draft");
}
/// A retained answer keeps the code-graph facts of the
/// window it quoted (rung `answer_window`, no new resolution) beside the
/// symbols its quote resolves to (rung `quote`).
#[tokio::test]
async fn a_retained_answer_keeps_its_cited_windows_code_facts() {
    let f = Fixture::new();
    f.sync(&[
        (
            "a.py",
            "def alpha():\n    return 7\n\n\ndef beta():\n    return 8\n",
        ),
        (
            "b.py",
            "from a import alpha\n\ndef run():\n    return alpha()\n",
        ),
    ]);
    f.store
        .graph_project(&f.workspace, &f.key, &f.project)
        .unwrap();
    let answer = json!({"action":"answer","queries":[],"reads":[],"query":null,"source_id":null,"start_byte":null,"answer":"alpha returns 7.","evidence":[{"id":"file:a.py","quote":"    return 7"}],"unresolved":[]});
    let remote = mock(vec![answer], None).await;
    let result = run(
        &f,
        &remote,
        json!({"kind":"reflect","question":"What does alpha return?","retain":true,"max_calls":1,"verify_answer":false}),
    )
    .await;
    assert_eq!(result["state"], "complete", "{result}");
    let retrieval = &result["result"]["context"]["retrievals"][0];
    assert_eq!(retrieval["code_graph"]["status"], "fused", "{retrieval:#}");
    let draft = f
        .memories()
        .into_iter()
        .find(|m| m["kind"] == "reflection")
        .unwrap();
    let links = draft["data"]["symbols"].as_array().unwrap().clone();
    let quoted = links
        .iter()
        .find(|l| l["rung"] == "quote")
        .unwrap_or_else(|| panic!("{links:#?}"));
    assert_eq!(quoted["identity"]["qualified_name"], "alpha");
    let window = links
        .iter()
        .find(|l| l["rung"] == "answer_window")
        .unwrap_or_else(|| panic!("{links:#?}"));
    assert_eq!(window["state"], "resolved");
    assert_eq!(window["identity"]["relpath"], "a.py");
    assert_eq!(window["identity"]["ordinal"], 0);
    assert!(
        window["revision"].as_str().is_some_and(|r| r.len() == 64),
        "{window:#}"
    );
    assert_eq!(draft["data"]["symbol_files"], json!({"a.py":"symbols"}));
}
#[tokio::test]
async fn refresh_preserves_blocks_and_approval_updates_the_original_revision() {
    let f = Fixture::new();
    f.sync(&[("notes.txt", "The room is blue.")]);
    let note=f.store.note(&f.workspace,&f.key,&f.args(json!({"title":"Room","body":"## Details\n\nThe room is blue.\n\nKeep the entrance clear.","dependencies":["notes.txt"]}))).unwrap();
    f.sync(&[("notes.txt", "The room is green.")]);
    let context = f
        .store
        .refresh_context(
            &f.workspace,
            &f.key,
            &f.args(json!({"memory_id":note["id"]})),
        )
        .unwrap();
    let section = &context["context"]["document"]["sections"][0];
    let response = json!({"operations":[{"op":"replace_block","section_id":section["id"],"block_id":section["blocks"][0]["id"],"text":"The room is green."}],"evidence":[{"id":"file:notes.txt","quote":"The room is green."}]});
    let remote = mock(vec![response], None).await;
    let result = run(
        &f,
        &remote,
        json!({"kind":"refresh","memory_id":note["id"]}),
    )
    .await;
    assert_eq!(result["state"], "complete", "{result}");
    let draft = f
        .memories()
        .into_iter()
        .find(|m| m["state"] == "draft")
        .unwrap();
    let changed = f.approve(&draft);
    assert_eq!(changed["id"], note["id"]);
    assert_eq!(changed["revision"], 2);
    assert!(s(&changed, "body").contains("Keep the entrance clear."));
    let history = f
        .store
        .project_read(
            &f.workspace,
            &f.key,
            "memory.history",
            &f.args(json!({"memory_id":note["id"]})),
        )
        .unwrap();
    assert_eq!(history["versions"].as_array().unwrap().len(), 2);
}
#[tokio::test]
async fn refresh_asks_once_more_when_an_operation_names_a_missing_block() {
    for calls in [2, 1] {
        let f = Fixture::new();
        f.sync(&[("notes.txt", "The room is blue.")]);
        let note=f.store.note(&f.workspace,&f.key,&f.args(json!({"title":"Room","body":"## Details\n\nThe room is blue.\n\nKeep the entrance clear.","dependencies":["notes.txt"]}))).unwrap();
        f.sync(&[("notes.txt", "The room is green.")]);
        let context = f
            .store
            .refresh_context(
                &f.workspace,
                &f.key,
                &f.args(json!({"memory_id":note["id"]})),
            )
            .unwrap();
        let section = &context["context"]["document"]["sections"][0];
        let block = s(&section["blocks"][0], "id").to_string();
        let evidence = json!([{"id":"file:notes.txt","quote":"The room is green."}]);
        let typo = json!({"operations":[{"op":"replace_block","section_id":section["id"],"block_id":"b-typo","text":"The room is green."}],"evidence":evidence});
        let fixed = json!({"operations":[{"op":"replace_block","section_id":section["id"],"block_id":block,"text":"The room is green."}],"evidence":evidence});
        let remote = mock(vec![typo, fixed], None).await;
        let result = run(
            &f,
            &remote,
            json!({"kind":"refresh","memory_id":note["id"],"max_calls":calls}),
        )
        .await;
        let requests = remote.state.requests.lock().unwrap().clone();
        if calls == 2 {
            assert_eq!(result["state"], "complete", "{result}");
            assert_eq!(requests.len(), 2);
            // The second ask quotes the refusal and lists the real block ids.
            let second = requests[1].to_string();
            assert!(second.contains("unknown block_id: b-typo"), "{second}");
            assert!(second.contains(&block));
            let draft = f
                .memories()
                .into_iter()
                .find(|m| m["state"] == "draft")
                .unwrap();
            assert!(s(&draft, "body").contains("The room is green."));
        } else {
            // No call left: the refused reply fails the job, saying why.
            assert_eq!(requests.len(), 1);
            assert_eq!(result["state"], "failed", "{result}");
            assert!(
                s(&result, "error").contains("unknown block_id: b-typo"),
                "{result}"
            );
            assert!(f.memories().iter().all(|m| m["state"] != "draft"));
        }
    }
}
#[test]
fn reviewed_correction_retires_only_the_target_and_preserves_history() {
    let f = Fixture::new();
    f.sync(&[(
        "notes.txt",
        "Mira prefers quiet rooms. Mira now prefers sunny rooms.",
    )]);
    let propose = |value: Value, targets: Vec<Value>| {
        f.store
            .authorized(&f.workspace, &f.key, "propose", |db, p| {
                let repo = graphicle_runtime::store::repo(db, p, &f.project)?;
                let docs = memory::documents(db, &repo, p)?;
                let sources = docs
                    .iter()
                    .map(|d| json!({"id":d["id"],"text":d["content"],"sha256":d["sha256"]}))
                    .collect::<Vec<_>>();
                claims::propose(
                    db,
                    &repo,
                    p,
                    &json!([value]),
                    &sources,
                    &targets,
                    &json!({"model":"review fixture"}),
                )
            })
            .unwrap()
    };
    propose(claim("Mira prefers quiet rooms.", "ADD", None), vec![]);
    f.approve(&f.memories()[0]);
    let targets = f
        .store
        .authorized(&f.workspace, &f.key, "read", |db, p| {
            claims::current(db, &graphicle_runtime::store::repo(db, p, &f.project)?, p)
        })
        .unwrap();
    let mut corrected = claim(
        "Mira now prefers sunny rooms.",
        "CORRECT",
        Some(s(&targets[0], "id")),
    );
    corrected["object"] = json!("sunny rooms");
    propose(corrected, targets.clone());
    let draft = f
        .memories()
        .into_iter()
        .find(|m| m["state"] == "draft")
        .unwrap();
    f.approve(&draft);
    let memories = f.memories();
    assert_eq!(
        memories.iter().filter(|m| m["state"] == "retired").count(),
        1
    );
    assert_eq!(
        memories
            .iter()
            .filter(|m| m["state"] == "reviewed" && m["stale"] == false)
            .count(),
        1,
        "{memories:?}"
    );
}
#[test]
fn private_sources_do_not_leak_through_memory_children_or_history() {
    let f = Fixture::new();
    let connector=f.store.connector_add(&f.workspace,&f.key,&f.args(json!({"kind":"jsonl","name":"Private","config":{"acl":{"workspace_readers":false,"credentials":[]}}}))).unwrap();
    f.store.import_documents(&f.workspace,&f.key,&f.args(json!({"connector_id":connector["id"],"documents":[{"id":"secret","title":"Private","content":"Confidential project detail."}]}))).unwrap();
    let source = format!("connector:{}:secret", s(&connector, "id"));
    f.store.authorized(&f.workspace,&f.key,"approve",|db,_|{let parent=memory::save(db,&f.project,None,None,"note","Private note","Confidential project detail.","reviewed",&json!({"dependencies":{},"evidence_links":{},"native_sources":{source:graphicle_memory::text::sha256(b"Confidential project detail.")},"notes":{}}))?;
        memory::save(db,&f.project,None,None,"context","Derived note","Also confidential.","reviewed",&json!({"dependencies":{},"evidence_links":{},"native_sources":{},"notes":{s(&parent,"id"):1}}))?;Ok(())}).unwrap();
    let reader = f
        .store
        .grant(
            &f.workspace,
            &f.key,
            &json!({"role":"reader","label":"Reader"}),
        )
        .unwrap();
    assert_eq!(f.memories().len(), 2);
    let seen = f
        .store
        .project_read(
            &f.workspace,
            s(&reader, "capability"),
            "memories",
            &f.args(json!({"include_retired":true})),
        )
        .unwrap();
    assert!(seen.as_array().unwrap().is_empty());
    let packet = f
        .store
        .project_context(
            &f.workspace,
            s(&reader, "capability"),
            &f.args(json!({"question":"confidential detail"})),
        )
        .unwrap();
    assert!(!s(&packet, "markdown").contains("Confidential project"));
}
#[test]
fn snapshot_import_tombstones_only_after_a_valid_batch() {
    let f = Fixture::new();
    let source=f.store.connector_add(&f.workspace,&f.key,&f.args(json!({"kind":"jsonl","name":"Docs","config":{"acl":{"workspace_readers":true,"credentials":[]}}}))).unwrap();
    let args = |docs: Value| {
        f.args(json!({"connector_id":source["id"],"mode":"snapshot","documents":docs}))
    };
    f.store.import_documents(&f.workspace,&f.key,&args(json!([{"id":"one","title":"One","content":"first"},{"id":"two","title":"Two","content":"second"}]))).unwrap();
    assert!(f.store.import_documents(&f.workspace,&f.key,&args(json!([{"id":"one","title":"One","content":"changed"},{"id":"two","title":"Two","content":"x","source_timestamp":"yesterday"}]))).is_err());
    let files = f
        .store
        .project_read(&f.workspace, &f.key, "files", &f.args(json!({})))
        .unwrap();
    assert_eq!(files["total"], 2);
    f.store
        .import_documents(
            &f.workspace,
            &f.key,
            &args(json!([{"id":"one","title":"One","content":"changed"}])),
        )
        .unwrap();
    assert_eq!(
        f.store
            .project_read(&f.workspace, &f.key, "files", &f.args(json!({})))
            .unwrap()["total"],
        1
    );
}
#[test]
fn local_sync_excludes_secret_files_and_does_not_follow_symlinks() {
    let f = Fixture::new();
    let root = TempDir::new().unwrap();
    std::fs::write(root.path().join("README.md"), "Public notes").unwrap();
    std::fs::write(root.path().join(".env"), "SECRET=never-import").unwrap();
    std::fs::write(root.path().join(".gitignore"), "ignored.txt\n").unwrap();
    std::fs::write(root.path().join("ignored.txt"), "ignored").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("README.md", root.path().join("alias.txt")).unwrap();
    let mut session = local::Session {
        workspace: f.workspace.clone(),
        capability: f.key.clone(),
        project: Some(f.project.clone()),
    };
    local::sync(&f.store, &mut session, root.path(), Some(&f.project)).unwrap();
    let files = f
        .store
        .project_read(&f.workspace, &f.key, "files", &f.args(json!({})))
        .unwrap();
    let text = files.to_string();
    assert!(text.contains("README.md"));
    assert!(!text.contains(".env"));
    assert!(!text.contains("ignored.txt"));
    assert!(!text.contains("alias.txt"));
}
#[test]
fn mcp_scope_is_fixed_and_no_tool_can_approve_memory() {
    let f = Fixture::new();
    let app = api::App::new(f.store.clone(), model::Config::default(), vec![]).unwrap();
    let tools = api::mcp_tools(true);
    assert!(
        !tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| s(t, "name").contains("approve"))
    );
    let reply=api::mcp(&app,&f.workspace,&f.key,&f.project,&json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"recall_project","arguments":{"repository_id":"other","question":"anything"}}})).unwrap();
    assert!(reply.get("error").is_some());
    assert!(
        api::mcp(
            &app,
            &f.workspace,
            &f.key,
            &f.project,
            &json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        )
        .is_none()
    );
}

#[tokio::test]
async fn embeddings_feed_query_ranking_and_reuse_query_vectors() {
    let f = Fixture::new();
    f.sync(&[
        ("notes.txt", "Mira prefers quiet rooms."),
        ("noise.txt", "The loud generator runs outside."),
    ]);
    let mut remote = mock(vec![], None).await;
    remote.config.embedding_model = Some("fixture".into());
    let indexed = run(&f, &remote, json!({"kind":"embed","max_calls":1})).await;
    assert_eq!(indexed["state"], "complete", "{indexed}");
    assert_eq!(indexed["result"]["embedded_passages"], 2);
    assert_eq!(indexed["calls"][0]["usage_reported"], 1);
    let packet = run(
        &f,
        &remote,
        json!({"kind":"recall","question":"peaceful place","max_calls":1}),
    )
    .await;
    assert_eq!(packet["state"], "complete", "{packet}");
    assert_eq!(packet["result"]["semantic"]["loaded"], true);
    assert!(s(&packet["result"], "markdown").contains("quiet rooms"));
    let reused = run(
        &f,
        &remote,
        json!({"kind":"recall","question":"peaceful place","max_calls":2}),
    )
    .await;
    assert_eq!(reused["state"], "complete", "{reused}");
    assert_eq!(reused["calls"][0]["cached"], 1);
    assert_eq!(remote.state.count.load(Ordering::SeqCst), 2);
    f.sync(&[("notes.txt", "Mira prefers sunny rooms.")]);
    let outdated = run(
        &f,
        &remote,
        json!({"kind":"recall","question":"peaceful place","max_calls":1}),
    )
    .await;
    assert_eq!(outdated["result"]["semantic"]["loaded"], false);
}

#[tokio::test]
async fn basic_answers_use_configured_semantic_search_and_can_disable_it() {
    let f = Fixture::new();
    f.sync(&[
        ("notes.txt", "Mira prefers quiet rooms."),
        ("noise.txt", "The loud generator runs outside."),
    ]);
    let answer = json!({"answer":"Mira prefers quiet rooms.","abstain":false,"evidence":[{"id":"file:notes.txt","quote":"Mira prefers quiet rooms."}],"unresolved":[]});
    let mut remote = mock(vec![answer], None).await;
    remote.config.embedding_model = Some("fixture".into());
    assert_eq!(
        run(&f, &remote, json!({"kind":"embed","max_calls":1})).await["state"],
        "complete"
    );
    let result = run(
        &f,
        &remote,
        json!({"kind":"basic","question":"peaceful place","max_calls":2}),
    )
    .await;
    assert_eq!(result["state"], "complete", "{result}");
    assert_eq!(result["result"]["context"]["semantic"]["loaded"], true);
    assert_eq!(remote.state.count.load(Ordering::SeqCst), 3);
    let job=f.store.enqueue(&f.workspace,&f.key,&f.args(json!({"kind":"basic","question":"peaceful place","semantic_retrieval":false,"max_calls":1})),&remote.config).unwrap();
    f.store
        .transaction(|db| {
            let payload: String = db.query_row(
                "SELECT payload FROM am_jobs WHERE id=?",
                [s(&job, "id")],
                |r| r.get(0),
            )?;
            let payload: Value = serde_json::from_str(&payload)?;
            assert!(payload["embedding_key"].is_null());
            Ok(())
        })
        .unwrap();
}

#[tokio::test]
async fn gather_searches_complementary_sources_with_one_controller_call() {
    let f = Fixture::new();
    f.sync(&[
        ("capacity.txt", "The meeting room capacity is 24."),
        ("access.txt", "The room has step-free access."),
        ("menu.txt", "The caterer has a vegan menu."),
    ]);
    let gather = json!({"action":"gather","queries":["room capacity","step-free access","vegan menu"],"reads":[],"query":null,"source_id":null,"start_byte":null,"answer":"","evidence":[],"unresolved":[]});
    let answer = json!({"action":"answer","queries":[],"reads":[],"query":null,"source_id":null,"start_byte":null,"answer":"The room holds 24 and has step-free access; the caterer has a vegan menu.","evidence":[{"id":"file:capacity.txt","quote":"The meeting room capacity is 24."},{"id":"file:access.txt","quote":"The room has step-free access."},{"id":"file:menu.txt","quote":"The caterer has a vegan menu."}],"unresolved":[]});
    let remote = mock(vec![gather, answer], None).await;
    let result=run(&f,&remote,json!({"kind":"reflect","question":"Can we host 20 people with mobility and dietary requirements?","context_tokens":1500,"max_calls":2,"verify_answer":false})).await;
    assert_eq!(result["state"], "complete", "{result}");
    assert_eq!(remote.state.count.load(Ordering::SeqCst), 2);
    assert_eq!(
        result["result"]["trace"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v["action"] == "search")
            .count(),
        3
    );
    assert!(
        result["result"]["context"]["evidence_tokens"]
            .as_u64()
            .unwrap()
            <= 1500
    );
}

#[tokio::test]
async fn reflection_receives_actual_reviewed_memory_text() {
    let f = Fixture::new();
    f.sync(&[("notes.txt", "Mira prefers quiet rooms.")]);
    f.store
        .authorized(&f.workspace, &f.key, "approve", |db, _| {
            memory::save(
                db,
                &f.project,
                None,
                None,
                "decision",
                "Workshop context",
                "Preserve Mira's quiet-room requirement.",
                "reviewed",
                &json!({"dependencies":{},"native_sources":{},"evidence_links":{},"notes":{}}),
            )
        })
        .unwrap();
    let response = json!({"action":"answer","queries":[],"reads":[],"query":null,"source_id":null,"start_byte":null,"answer":"Quiet rooms.","evidence":[{"id":"file:notes.txt","quote":"Mira prefers quiet rooms."}],"unresolved":[]});
    let remote = mock(vec![response], None).await;
    let job = run(
        &f,
        &remote,
        json!({"kind":"reflect","question":"Mira quiet-room requirement","max_calls":1}),
    )
    .await;
    assert_eq!(job["state"], "complete", "{job}");
    let request = remote.state.requests.lock().unwrap()[0].clone();
    let input: Value = serde_json::from_str(s(&request, "input")).unwrap();
    assert!(
        input["reviewed_memory"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["body"] == "Preserve Mira's quiet-room requirement.")
    );
}

#[test]
fn bitemporal_history_does_not_learn_future_supersession_early() {
    let f = Fixture::new();
    f.sync(&[(
        "notes.txt",
        "2026-09-01: Mira prefers quiet rooms. 2026-09-10: Mira now prefers sunny rooms.",
    )]);
    let prepared = f
        .store
        .interpretation(&f.workspace, &f.key, &f.args(json!({})), false)
        .unwrap();
    let mut first = claim("Mira prefers quiet rooms.", "ADD", None);
    first["valid_at"] = json!("2026-09-01T00:00:00Z");
    first["time_basis"] = json!("explicit");
    f.store
        .interpretation(
            &f.workspace,
            &f.key,
            &f.args(json!({"context_sha256":prepared["sha256"],"claims":[first]})),
            true,
        )
        .unwrap();
    f.approve(&f.memories()[0]);
    let prepared = f
        .store
        .interpretation(&f.workspace, &f.key, &f.args(json!({})), false)
        .unwrap();
    let target = &prepared["context"]["targets"][0];
    let known = target["data"]["approved_at"].clone();
    let mut next = claim(
        "Mira now prefers sunny rooms.",
        "SUPERSEDE",
        Some(s(target, "id")),
    );
    next["object"] = json!("sunny rooms");
    next["valid_at"] = json!("2026-09-10T00:00:00Z");
    next["time_basis"] = json!("explicit");
    f.store
        .interpretation(
            &f.workspace,
            &f.key,
            &f.args(json!({"context_sha256":prepared["sha256"],"claims":[next]})),
            true,
        )
        .unwrap();
    let draft = f
        .memories()
        .into_iter()
        .find(|m| m["state"] == "draft")
        .unwrap();
    f.approve(&draft);
    let historical = f
        .store
        .claim_history(
            &f.workspace,
            &f.key,
            &f.args(json!({"as_of":"2026-09-15T00:00:00Z","known_at":known})),
        )
        .unwrap();
    assert_eq!(
        historical["claims"].as_array().unwrap().len(),
        1,
        "{historical}"
    );
    assert_eq!(historical["claims"][0]["data"]["object"], "quiet rooms");
    assert!(historical["claims"][0]["data"]["invalid_at"].is_null());
    let current = f
        .store
        .claim_history(
            &f.workspace,
            &f.key,
            &f.args(json!({"as_of":"2026-09-15T00:00:00Z"})),
        )
        .unwrap();
    assert_eq!(current["claims"].as_array().unwrap().len(), 1, "{current}");
    assert_eq!(current["claims"][0]["data"]["object"], "sunny rooms");
}

#[test]
fn source_snapshot_pin_backup_restore_and_forgetting_are_real() {
    let f = Fixture::new();
    f.sync(&[("notes.txt", "Old original.")]);
    let before = f
        .store
        .project_status(&f.workspace, &f.key, &f.args(json!({})))
        .unwrap()["repository"]["snapshot"]
        .clone();
    f.sync(&[("notes.txt", "Current original.")]);
    let historical = f
        .store
        .read_project_source(
            &f.workspace,
            &f.key,
            &f.args(json!({"path":"notes.txt","snapshot":before})),
        )
        .unwrap();
    assert_eq!(historical["quote"], "Old original.");
    let backup = f.dir.path().join("backup.sqlite3");
    f.store.backup(&backup).unwrap();
    let restored = f.dir.path().join("restored");
    local::restore(&backup, &restored).unwrap();
    assert!(local::restore(&backup, &restored).is_err());
    let copy = Store::open(&restored).unwrap();
    assert_eq!(
        copy.read_project_source(&f.workspace, &f.key, &f.args(json!({"path":"notes.txt"})))
            .unwrap()["quote"],
        "Current original."
    );
    let connector=f.store.connector_add(&f.workspace,&f.key,&f.args(json!({"kind":"jsonl","name":"Private","config":{"acl":{"workspace_readers":false,"credentials":[]}}}))).unwrap();
    f.store.import_documents(&f.workspace,&f.key,&f.args(json!({"connector_id":connector["id"],"documents":[{"id":"private","title":"Private source","content":"Confidential source."}]}))).unwrap();
    let source = format!("connector:{}:private", s(&connector, "id"));
    f.store.authorized(&f.workspace,&f.key,"approve",|db,_|memory::save(db,&f.project,None,None,"note","Remember","Confidential source.","reviewed",&json!({"dependencies":{},"evidence_links":{},"notes":{},"native_sources":{source:graphicle_memory::text::sha256(b"Confidential source.")}}))).unwrap();
    let removed = f
        .store
        .maintain(
            &f.workspace,
            &f.key,
            "connector.delete",
            &f.args(json!({"connector_id":connector["id"]})),
        )
        .unwrap();
    assert_eq!(removed["derived_records_removed"], 1);
    assert!(f.memories().is_empty());
    assert_eq!(
        f.store
            .project_read(&f.workspace, &f.key, "files", &f.args(json!({})))
            .unwrap()["total"],
        1
    );
}

#[test]
fn expired_connector_snapshot_is_withheld_and_only_one_scan_can_own_a_cursor() {
    let f = Fixture::new();
    let connector=f.store.connector_add(&f.workspace,&f.key,&f.args(json!({"kind":"jsonl","name":"Source","config":{"acl":{"workspace_readers":true,"credentials":[]},"max_age_seconds":60}}))).unwrap();
    f.store.import_documents(&f.workspace,&f.key,&f.args(json!({"connector_id":connector["id"],"documents":[{"id":"old","title":"Remote","content":"Remote source text."}]}))).unwrap();
    f.store.transaction(|db| {db.execute("UPDATE am_connectors SET kind='github',last_sync='2020-01-01T00:00:00Z' WHERE id=?",[s(&connector,"id")])?;Ok(())}).unwrap();
    assert_eq!(
        f.store
            .project_read(&f.workspace, &f.key, "files", &f.args(json!({})))
            .unwrap()["total"],
        0
    );
    let config = model::Config::default();
    let first = f
        .store
        .enqueue(
            &f.workspace,
            &f.key,
            &f.args(json!({"kind":"connector","connector_id":connector["id"],"max_requests":1})),
            &config,
        )
        .unwrap();
    let second = f
        .store
        .enqueue(
            &f.workspace,
            &f.key,
            &f.args(json!({"kind":"connector","connector_id":connector["id"],"max_requests":20})),
            &config,
        )
        .unwrap();
    assert_eq!(first["id"], second["id"]);
    assert_eq!(second["replayed"], true);
}

#[tokio::test]
async fn empty_abstention_is_usable_without_inventing_an_answer() {
    let f = Fixture::new();
    f.sync(&[("notes.txt", "Mira prefers quiet rooms.")]);
    let remote=mock(vec![json!({"action":"abstain","queries":[],"reads":[],"query":null,"source_id":null,"start_byte":null,"answer":"","evidence":[],"unresolved":["No passport number in these sources."]})],None).await;
    let job=run(&f,&remote,json!({"kind":"reflect","question":"What is Mira's passport number?","max_calls":1,"retain":true})).await;
    assert_eq!(job["state"], "complete", "{job}");
    assert_eq!(job["result"]["abstain"], true);
    assert!(s(&job["result"], "answer").contains("do not answer"));
    assert!(f.memories().is_empty());
}

#[test]
fn recurring_source_sync_is_explicit_bounded_and_pauseable() {
    let f = Fixture::new();
    let connector=f.store.connector_add(&f.workspace,&f.key,&f.args(json!({"kind":"github","name":"Source","credential_env":"GRAPHICLE_FIXTURE_CREDENTIAL","config":{"repository":"team/source","acl":{"workspace_readers":false,"credentials":[]}}}))).unwrap();
    assert_eq!(f.store.schedule_connectors().unwrap(), 0);
    f.store
        .connector_schedule(
            &f.workspace,
            &f.key,
            &f.args(json!({"connector_id":connector["id"],"sync_every_seconds":900})),
        )
        .unwrap();
    assert_eq!(f.store.schedule_connectors().unwrap(), 1);
    assert_eq!(f.store.schedule_connectors().unwrap(), 0);
    let job = f.store.lease_job().unwrap().unwrap();
    assert_eq!(job.payload["max_calls"], 0);
    assert_eq!(job.payload["max_requests"], 64);
    f.store
        .finish_job(&job, Ok(json!({"complete":false})))
        .unwrap();
    f.store
        .maintain(
            &f.workspace,
            &f.key,
            "connector.state",
            &f.args(json!({"connector_id":connector["id"],"state":"paused"})),
        )
        .unwrap();
    assert_eq!(f.store.schedule_connectors().unwrap(), 0);
}

#[tokio::test]
async fn answer_reviewer_corrects_incomplete_answer_and_counts_both_calls() {
    let f = Fixture::new();
    f.sync(&[("notes.txt", "Mira is vegetarian. Arun cannot have dairy.")]);
    let candidate = json!({"action":"answer","queries":[],"reads":[],"query":null,"source_id":null,"start_byte":null,"answer":"Mira is vegetarian.","evidence":[{"id":"file:notes.txt","quote":"Mira is vegetarian."}],"unresolved":[]});
    let review = json!({"reviewed_answer":{"answer":"Mira is vegetarian; Arun needs dairy-free food.","abstain":false,"evidence":[{"id":"file:notes.txt","quote":"Mira is vegetarian. Arun cannot have dairy."}],"unresolved":[]},"coverage":[{"requested_part":"Mira's food","status":"answered","evidence":[{"id":"file:notes.txt","quote":"Mira is vegetarian."}]},{"requested_part":"Arun's food","status":"answered","evidence":[{"id":"file:notes.txt","quote":"Arun cannot have dairy."}]}]});
    let remote = mock(vec![candidate, review], None).await;
    let done = run(
        &f,
        &remote,
        json!({"kind":"reflect","question":"What food do Mira and Arun each need?","max_calls":2}),
    )
    .await;
    assert_eq!(done["state"], "complete", "{done}");
    assert!(s(&done["result"], "answer").contains("Arun"));
    assert_eq!(done["result"]["review"]["status"], "completed");
    assert_eq!(done["result"]["review"]["semantic_proof"], false);
    assert_eq!(remote.state.count.load(Ordering::SeqCst), 2);
    let requests = remote.state.requests.lock().unwrap();
    assert_eq!(
        requests[0]["text"]["format"]["schema"]["properties"]["action"]["enum"],
        json!(["answer", "abstain"])
    );
}
#[tokio::test]
async fn invalid_review_preserves_grounded_candidate_but_does_not_retain_it() {
    let f = Fixture::new();
    f.sync(&[("notes.txt", "Mira is vegetarian.")]);
    let candidate = json!({"action":"answer","queries":[],"reads":[],"query":null,"source_id":null,"start_byte":null,"answer":"Mira is vegetarian.","evidence":[{"id":"file:notes.txt","quote":"Mira is vegetarian."}],"unresolved":[]});
    let invalid = json!({"reviewed_answer":{"answer":"Mira is dairy free.","abstain":false,"evidence":[{"id":"file:notes.txt","quote":"Mira is dairy free."}],"unresolved":[]},"coverage":[{"requested_part":"Food","status":"answered","evidence":[{"id":"file:notes.txt","quote":"Mira is dairy free."}]}]});
    let remote = mock(vec![candidate, invalid], None).await;
    let done=run(&f,&remote,json!({"kind":"reflect","question":"What food does Mira need?","max_calls":2,"retain":true})).await;
    assert_eq!(done["state"], "complete", "{done}");
    assert_eq!(done["result"]["answer"], "Mira is vegetarian.");
    assert_eq!(done["result"]["review"]["status"], "failed");
    assert!(f.memories().is_empty());
}

#[tokio::test]
async fn answer_review_repairs_a_nonverbatim_citation_within_the_existing_budget() {
    let f = Fixture::new();
    f.sync(&[("notes.txt", "Mira prefers quiet rooms.")]);
    let candidate = json!({"action":"answer","queries":[],"reads":[],"query":null,"source_id":null,"start_byte":null,"answer":"Mira prefers quiet rooms.","evidence":[{"id":"file:notes.txt","quote":"Mira prefers a quiet room."}],"unresolved":[]});
    let evidence = json!([{"id":"file:notes.txt","quote":"Mira prefers quiet rooms."}]);
    let review = json!({"reviewed_answer":{"answer":"Mira prefers quiet rooms.","abstain":false,"evidence":evidence,"unresolved":[]},"coverage":[{"requested_part":"Mira's room preference","status":"answered","evidence":evidence}]});
    let remote = mock(vec![candidate, review], None).await;
    let done = run(
        &f,
        &remote,
        json!({"kind":"reflect","question":"What room does Mira prefer?","max_calls":2}),
    )
    .await;
    assert_eq!(done["state"], "complete", "{done}");
    assert_eq!(done["result"]["evidence"], evidence);
    assert_eq!(
        done["result"]["review"]["candidate_citations_repaired"],
        true
    );
    assert_eq!(remote.state.count.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn invalid_candidate_is_never_returned_when_review_cannot_repair_it() {
    for mode in ["disabled", "exhausted", "failed"] {
        let f = Fixture::new();
        f.sync(&[("notes.txt", "Mira prefers quiet rooms.")]);
        let candidate = json!({"action":"answer","queries":[],"reads":[],"query":null,"source_id":null,"start_byte":null,"answer":"Mira prefers quiet rooms.","evidence":[{"id":"file:notes.txt","quote":"Mira prefers a quiet room."}],"unresolved":[]});
        let invalid = json!({"reviewed_answer":{"answer":"Mira prefers quiet rooms.","abstain":false,"evidence":[{"id":"file:notes.txt","quote":"Mira prefers a quiet room."}],"unresolved":[]},"coverage":[{"requested_part":"Room preference","status":"answered","evidence":[{"id":"file:notes.txt","quote":"Mira prefers a quiet room."}]}]});
        let remote = mock(vec![candidate, invalid], None).await;
        let done = run(&f, &remote, json!({"kind":"reflect","question":"What room does Mira prefer?","max_calls":if mode=="exhausted"{1}else{2},"verify_answer":mode!="disabled","retain":true})).await;
        assert_eq!(done["state"], "failed", "{mode}: {done}");
        assert!(s(&done, "error").contains("did not read"));
        assert_eq!(
            remote.state.count.load(Ordering::SeqCst),
            if mode == "failed" { 2 } else { 1 },
            "{mode}"
        );
        assert!(f.memories().is_empty());
    }
}

#[tokio::test]
async fn explicit_retry_preserves_failure_usage_and_repeated_clicks_reuse_the_child() {
    let f = Fixture::new();
    f.sync(&[("notes.txt", "Mira prefers quiet rooms.")]);
    let remote=mock(vec![json!({"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":20,"output_tokens":64},"output":[]}),json!({"answer":"Mira prefers quiet rooms.","abstain":false,"evidence":[{"id":"file:notes.txt","quote":"Mira prefers quiet rooms."}],"unresolved":[]})],None).await;
    let failed = run(
        &f,
        &remote,
        json!({"kind":"basic","question":"What does Mira prefer?","max_calls":1}),
    )
    .await;
    assert_eq!(failed["state"], "failed");
    assert!(
        s(&failed, "error").contains("max_output_tokens"),
        "{failed}"
    );
    assert_eq!(failed["calls"][0]["input_tokens"], 20);
    assert_eq!(failed["calls"][0]["output_tokens"], 64);
    assert_eq!(failed["calls"][0]["usage_reported"], 1);
    assert_eq!(failed["calls"][0]["call_state"], "failed");
    let arguments = f.args(json!({"job_id":failed["id"]}));
    let child = f
        .store
        .retry_job(&f.workspace, &f.key, &arguments, &remote.config)
        .unwrap();
    let again = f
        .store
        .retry_job(&f.workspace, &f.key, &arguments, &remote.config)
        .unwrap();
    assert_eq!(child["id"], again["id"]);
    Worker::new(f.store.clone(), remote.config.clone())
        .unwrap()
        .once()
        .await
        .unwrap();
    assert_eq!(f.status(s(&child, "id"))["state"], "complete");
    assert_eq!(f.status(s(&failed, "id"))["state"], "failed");
    assert_eq!(remote.state.count.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn chat_failures_preserve_reported_usage_without_committing_partial_memory() {
    for (finish, refusal, reason) in [
        ("length", None, "max_output_tokens"),
        ("content_filter", None, "content_filter"),
        ("stop", Some("private provider refusal text"), "refusal"),
    ] {
        let f = Fixture::new();
        f.sync(&[("notes.txt", "Mira prefers quiet rooms.")]);
        let mut remote = mock(
            vec![
                json!({"model":"fixture-model-snapshot","choices":[{"finish_reason":finish,
                "message":{"content":"partial and untrusted","refusal":refusal}}],
                "usage":{"prompt_tokens":20,"completion_tokens":64}}),
            ],
            None,
        )
        .await;
        remote.config.models.get_mut("fixture").unwrap().protocol = "azure_chat".into();
        let failed = run(&f, &remote, json!({"kind":"extract","max_calls":1})).await;
        assert_eq!(failed["state"], "failed", "{failed}");
        assert!(s(&failed, "error").contains(reason), "{failed}");
        assert!(!s(&failed, "error").contains("private provider refusal text"));
        let call = &failed["calls"][0];
        assert_eq!(call["input_tokens"], 20);
        assert_eq!(call["output_tokens"], 64);
        assert_eq!(call["model"], "fixture-model-snapshot");
        assert_eq!(call["usage_reported"], 1);
        assert_eq!(call["call_state"], "failed");
        assert!(f.memories().is_empty());
    }
}
