//! The grounded-v3 reader over observer notes alone: every note in context
//! when they fit, time-ordered excerpts, mended citations, and its notes
//! budget beside the evidence. (Observer reflection and the reader options
//! built on it are not supported; tests/it/archived.rs checks they are
//! refused.)
use axum::{Json, Router, extract::State, routing::post};
use graphicle_runtime::{
    jobs::Worker,
    model,
    store::{Store, s},
};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

struct Fixture {
    _dir: TempDir,
    store: Store,
    workspace: String,
    key: String,
    repo: String,
}
impl Fixture {
    fn new(files: &[(&str, &str)]) -> Self {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let ws = store.create_workspace("Grounded v3").unwrap();
        let workspace = s(&ws, "id").to_string();
        let key = s(&ws, "capability").to_string();
        let repo = s(
            &store
                .register(
                    &workspace,
                    &key,
                    &json!({"project_key":"grounded-v3","name":"Observed sessions"}),
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
    async fn observe(&self, m: &Provider) {
        let observed = self
            .run(
                m,
                json!({"kind":"observer_notes","include":["file:session-"],"max_calls":4}),
            )
            .await;
        assert_eq!(observed["result"]["complete"], true, "{observed}");
    }
}

#[derive(Clone, Default)]
struct Seen {
    /// Whole answer requests: instructions and decoded input.
    answers: Arc<Mutex<Vec<Value>>>,
    /// How the next answers reply; "cite" when empty.
    script: Arc<Mutex<VecDeque<&'static str>>>,
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

fn user_line(text: &str) -> Option<&str> {
    text.lines()
        .find_map(|l| l.strip_prefix("user: "))
        .map(str::trim)
}

fn mentioning<'a>(sources: &'a [Value], word: &str) -> &'a Value {
    sources
        .iter()
        .find(|src| s(src, "text").contains(word))
        .unwrap_or_else(|| panic!("no source mentions {word}: {sources:?}"))
}

fn answer(mode: &str, input: &Value, v3: bool) -> Value {
    let sources = input["sources"].as_array().cloned().unwrap_or_default();
    let cite = |src: &Value| json!({"id":src["id"],"quote":user_line(s(src, "text")).unwrap()});
    let (answer, evidence) = match mode {
        // A decorated quote, an exact quote under the wrong id, an invention.
        "repair" => {
            let (fitbit, luna) = (mentioning(&sources, "Fitbit"), mentioning(&sources, "Luna"));
            let wrong = user_line(s(luna, "text")).unwrap();
            (
                "Fitbit Inspire HR; Luna saw the vet on 2023/05/27.",
                json!([
                    {"id":fitbit["id"],"quote":format!("\u{201c}{}\u{201d}", user_line(s(fitbit, "text")).unwrap())},
                    {"id":fitbit["id"],"quote":wrong},
                    {"id":fitbit["id"],"quote":"An invented line nobody wrote."}
                ]),
            )
        }
        _ => {
            let src = sources
                .iter()
                .find(|src| s(src, "text").contains("Luna"))
                .or(sources.first())
                .unwrap();
            ("fixture answer", json!([cite(src)]))
        }
    };
    let mut reply = json!({"answer":answer,"abstain":false,"evidence":evidence,"unresolved":[]});
    if v3 {
        reply["reading_notes"] =
            json!([{"source":"observer_notes","date":"","note":"fixture reading"}]);
        reply["premise_holds"] = json!(true);
    }
    reply
}

async fn inference(State(seen): State<Seen>, Json(request): Json<Value>) -> Json<Value> {
    let input: Value = serde_json::from_str(request["input"].as_str().unwrap()).unwrap();
    let result = if let Some(session) = input["session"].as_str() {
        let line = user_line(session).unwrap();
        json!({"notes":[{"text":format!("[{}] {line}", s(&input, "session_date")),"quotes":[line]}]})
    } else if input["question"].is_string() {
        let instructions = s(&request, "instructions").to_string();
        seen.answers
            .lock()
            .unwrap()
            .push(json!({"instructions":instructions,"input":input}));
        let mode = seen.script.lock().unwrap().pop_front().unwrap_or("cite");
        answer(mode, &input, instructions.contains("reading_notes"))
    } else {
        panic!("Unexpected model request {input}")
    };
    Json(
        json!({"status":"completed","model":"fixture","usage":{"input_tokens":150,"output_tokens":80},"output":[{"type":"message","content":[{"type":"output_text","text":result.to_string()}]}]}),
    )
}

async fn provider() -> Provider {
    let seen = Seen::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/v1/responses", post(inference))
        .with_state(seen.clone());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let config=serde_json::from_value(json!({"default_model":"fixture","models":{"fixture":{"protocol":"responses","base_url":format!("http://{address}/v1"),"model":"fixture","input_limit":16000,"output_limit":4096}}})).unwrap();
    Provider { config, seen, task }
}

const S1: &str = "Conversation date: 2023/05/20 (Sat) 02:21\n\nuser: I bought a Fitbit Inspire HR last week.\nassistant: Nice choice.\n";
const S2: &str = "Conversation date: 2023/05/27 (Sat) 10:00\n\nuser: My cat Luna had her vet visit today.\nassistant: I hope she is well.\n";

#[tokio::test]
async fn grounded_v3_reads_every_note_in_time_order_and_mends_citations() {
    // Path order is the reverse of session order.
    let f = Fixture::new(&[("session-b.txt", S1), ("session-a.txt", S2)]);
    let m = provider().await;
    f.observe(&m).await;
    m.seen.script.lock().unwrap().push_back("repair");
    let answered = f
        .run(
            &m,
            json!({"kind":"basic","question":"Which Fitbit did I buy, and when did Luna see the vet?",
                "answer_contract":"grounded-v3","max_calls":1}),
        )
        .await;
    assert_eq!(answered["state"], "complete", "{answered}");
    let result = &answered["result"];
    let context = &result["context"];
    assert_eq!(context["answer_contract"], "grounded-v3");
    let notes = &context["observer_notes"];
    assert_eq!(notes["route"], "all_notes", "{notes}");
    assert!(notes.get("links").is_none(), "{notes}");
    assert_eq!(
        (notes["available"].clone(), notes["selected"].clone()),
        (json!(2), json!(2))
    );
    assert_eq!(notes["citable"], false);
    assert_eq!(notes["authority"], "unreviewed_model_observation");
    assert_ne!(context["memory_profile"]["served"], true, "{context}");
    assert_eq!(context["chronological"], true);
    assert!(
        context["total_context_tokens"].as_u64().unwrap()
            > context["evidence_tokens"].as_u64().unwrap()
    );
    assert!(context.get("user_preferences").is_none(), "{context}");

    let request = m.seen.answers.lock().unwrap().last().unwrap().clone();
    let instructions = s(&request, "instructions");
    assert!(instructions.starts_with("Answer only from these original excerpts."));
    for rule in ["write reading_notes", "observer_notes are", "premise_holds"] {
        assert!(instructions.contains(rule), "{rule}: {instructions}");
    }
    for absent in [
        "memory_profile",
        "user_preferences",
        "Also report confidence",
    ] {
        assert!(!instructions.contains(absent), "{absent}: {instructions}");
    }
    let input = &request["input"];
    let keys: Vec<&str> = input
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys[..4],
        ["sources", "observer_notes", "question", "question_time"],
        "{keys:?}"
    );
    assert_eq!(keys.last(), Some(&"final_check"));
    // Every note, in session order.
    let sessions: Vec<&str> = input["observer_notes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| s(n, "source_id"))
        .collect();
    assert_eq!(sessions, ["file:session-b.txt", "file:session-a.txt"]);
    let ids: Vec<&str> = input["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| s(w, "id"))
        .collect();
    let first = |id: &str| ids.iter().position(|i| *i == id).unwrap();
    assert!(
        first("file:session-b.txt") < first("file:session-a.txt"),
        "the earlier session comes first: {ids:?}"
    );

    // The decorated quote is trimmed, the misattributed one moves to its
    // source and the invention is dropped.
    assert_eq!(result["citations_dropped"], 1, "{result}");
    assert_eq!(
        result["evidence"],
        json!([
            {"id":"file:session-b.txt","quote":"I bought a Fitbit Inspire HR last week."},
            {"id":"file:session-a.txt","quote":"My cat Luna had her vet visit today."}
        ])
    );
    assert_eq!(result["premise_holds"], true);
    assert_eq!(result["reading_notes"][0]["note"], "fixture reading");

    // Without notes the reader still gets the v3 contract.
    let bare = f
        .run(
            &m,
            json!({"kind":"basic","question":"When did Luna see the vet?",
                "answer_contract":"grounded-v3","observer_notes":false,"max_calls":1}),
        )
        .await;
    assert_eq!(bare["state"], "complete", "{bare}");
    assert!(bare["result"]["context"].get("observer_notes").is_none());
    let request = m.seen.answers.lock().unwrap().last().unwrap().clone();
    let keys: Vec<&str> = request["input"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys[..2], ["sources", "question"], "{keys:?}");
    let instructions = s(&request, "instructions");
    assert!(
        instructions.contains("write reading_notes"),
        "{instructions}"
    );
    assert!(
        !instructions.contains("observer_notes are"),
        "{instructions}"
    );
}

#[tokio::test]
async fn grounded_v3_notes_budget_is_validated_when_queued() {
    let f = Fixture::new(&[("session-0001.txt", S1)]);
    let m = provider().await;
    for (args, error) in [
        (
            json!({"kind":"basic","question":"q","answer_contract":"grounded-v3","observer_notes":false,"notes_tokens":100}),
            "notes_tokens requires observer_notes",
        ),
        (
            json!({"kind":"basic","question":"q","answer_contract":"grounded-v3","notes_tokens":64001}),
            "notes_tokens",
        ),
        (
            json!({"kind":"basic","question":"q","observer_notes":true,"notes_tokens":20000}),
            "notes_tokens",
        ),
        (
            json!({"kind":"basic","question":"q","answer_contract":"grounded-v3","notes_state":"forged"}),
            "notes_state is pinned by the service",
        ),
    ] {
        let failed = f.enqueue(&m, args.clone()).unwrap_err().to_string();
        assert!(failed.contains(error), "{args}: {failed}");
    }
    // grounded-v3 reads notes by default, with a notes budget beside the
    // evidence that may exceed the one a grounded-v2 reader shares with it.
    let queued = f
        .enqueue(
            &m,
            json!({"kind":"basic","question":"q","answer_contract":"grounded-v3","notes_tokens":40000}),
        )
        .unwrap();
    assert!(queued["id"].is_string(), "{queued}");
    let payload: String = f
        .store
        .connect()
        .unwrap()
        .query_row(
            "SELECT payload FROM am_jobs WHERE id=?",
            [s(&queued, "id")],
            |r| r.get(0),
        )
        .unwrap();
    let payload: Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(payload["observer_notes"], true);
    assert!(payload["notes_state"].is_string(), "{payload}");
}
