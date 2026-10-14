//! Automatic observation (Mastra Observational Memory's threshold): a
//! connector `observe` policy queues observer rounds once unobserved source
//! tokens reach its threshold, chains budget-limited rounds and caps rounds per
//! generation. A reflection round is not supported (unit
//! `observer-reflection`).
use axum::{Json, Router, extract::State, routing::post};
use graphicle_runtime::{
    jobs::Worker,
    model,
    store::{Store, s},
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

#[derive(Clone, Default)]
struct Seen {
    observed: Arc<Mutex<Vec<String>>>,
}

async fn inference(State(seen): State<Seen>, Json(request): Json<Value>) -> Json<Value> {
    let input: Value = serde_json::from_str(request["input"].as_str().unwrap()).unwrap();
    let result = if let Some(session) = input["session"].as_str() {
        seen.observed
            .lock()
            .unwrap()
            .push(s(&input, "source_id").to_string());
        let line = session
            .lines()
            .find_map(|l| l.strip_prefix("user: "))
            .unwrap()
            .trim();
        json!({"notes":[{"text":format!("[{}] {line}", s(&input, "session_date")),"quotes":[line]}]})
    } else {
        panic!("Unexpected model request {input}")
    };
    Json(
        json!({"status":"completed","model":"fixture","usage":{"input_tokens":150,"output_tokens":80},"output":[{"type":"message","content":[{"type":"output_text","text":result.to_string()}]}]}),
    )
}

struct Fixture {
    _dir: TempDir,
    store: Store,
    workspace: String,
    key: String,
    repo: String,
    connector: String,
    config: model::Config,
    seen: Seen,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Fixture {
    async fn new(observe: Value) -> Self {
        let seen = Seen::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/v1/responses", post(inference))
            .with_state(seen.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let config = serde_json::from_value(json!({"default_model":"fixture","models":{"fixture":{"protocol":"responses","base_url":format!("http://{address}/v1"),"model":"fixture","input_limit":16000,"output_limit":4096}}})).unwrap();
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let ws = store.create_workspace("Automatic observation").unwrap();
        let workspace = s(&ws, "id").to_string();
        let key = s(&ws, "capability").to_string();
        let repo = s(
            &store
                .register(
                    &workspace,
                    &key,
                    &json!({"project_key":"auto","name":"Imported sessions"}),
                )
                .unwrap(),
            "id",
        )
        .to_string();
        let connector = s(
            &store
                .connector_add(
                    &workspace,
                    &key,
                    &json!({"repository_id":repo,"kind":"jsonl","name":"Sessions","config":{
                        "acl":{"workspace_readers":true,"credentials":[]},"observe":observe}}),
                )
                .unwrap(),
            "id",
        )
        .to_string();
        Self {
            _dir: dir,
            store,
            workspace,
            key,
            repo,
            connector,
            config,
            seen,
            task,
        }
    }
    fn import(&self, sessions: &[(&str, &str, &str)]) -> Value {
        let documents: Vec<Value> = sessions
            .iter()
            .map(|(id, date, line)| {
                json!({"id":id,"title":id,"content":format!("Conversation date: {date}\n\nuser: {line}\nassistant: Noted.\n")})
            })
            .collect();
        self.store
            .import_documents(
                &self.workspace,
                &self.key,
                &json!({"repository_id":self.repo,"connector_id":self.connector,"mode":"delta","documents":documents}),
            )
            .unwrap();
        self.decide()
    }
    fn decide(&self) -> Value {
        let decisions = self
            .store
            .auto_observe(&self.config, Some(&self.connector))
            .unwrap();
        assert_eq!(decisions.len(), 1, "{decisions:?}");
        decisions[0].clone()
    }
    fn set_policy(&self, observe: Value) -> Value {
        self.store
            .connector_observe(
                &self.workspace,
                &self.key,
                &json!({"repository_id":self.repo,"connector_id":self.connector,"observe":observe}),
            )
            .unwrap()
    }
    /// Run ready jobs until none is left (bounded).
    async fn drain(&self) {
        let worker = Worker::new(self.store.clone(), self.config.clone()).unwrap();
        let mut ran = 0;
        while worker.once().await.unwrap() {
            ran += 1;
            assert!(ran < 50, "automatic rounds did not settle");
        }
    }
    fn jobs(&self, kind: &str) -> Vec<Value> {
        let db = self.store.connect().unwrap();
        let mut statement = db
            .prepare("SELECT state,payload,result FROM am_jobs WHERE repository_id=? AND kind=? ORDER BY created_at,rowid")
            .unwrap();
        statement
            .query_map([self.repo.as_str(), kind], |r| {
                let payload: String = r.get(1)?;
                let result: Option<String> = r.get(2)?;
                Ok(json!({"state":r.get::<_, String>(0)?,
                    "payload":serde_json::from_str::<Value>(&payload).unwrap(),
                    "result":result.map(|text| serde_json::from_str::<Value>(&text).unwrap())}))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }
    fn noted_sources(&self) -> Vec<String> {
        let db = self.store.connect().unwrap();
        let mut statement = db
            .prepare(
                "SELECT source_id FROM am_observer_notes WHERE repository_id=? ORDER BY source_id",
            )
            .unwrap();
        statement
            .query_map([&self.repo], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }
}

const SESSIONS: [(&str, &str, &str); 3] = [
    (
        "s1",
        "2023/05/20 (Sat) 02:21",
        "I bought a Fitbit Inspire HR last week.",
    ),
    (
        "s2",
        "2023/05/27 (Sat) 10:00",
        "My cat Luna had her vet visit today.",
    ),
    (
        "s3",
        "2023/06/03 (Sat) 09:30",
        "We moved the team offsite to Lisbon.",
    ),
];

#[tokio::test]
async fn changing_the_observation_recipe_reprocesses_unchanged_sources() {
    let mut f = Fixture::new(json!({"threshold_tokens":1,"max_calls":1})).await;
    assert_eq!(f.import(&SESSIONS[..1])["queued"], true);
    f.drain().await;
    assert_eq!(f.seen.observed.lock().unwrap().len(), 1);
    assert_eq!(f.decide()["queued"], false);

    f.set_policy(json!({"threshold_tokens":1,"max_calls":1,"max_notes":2}));
    let changed = f.decide();
    assert_eq!(
        changed["queued"], true,
        "new note schema was ignored: {changed}"
    );
    f.drain().await;
    assert_eq!(f.seen.observed.lock().unwrap().len(), 2);
    assert_eq!(f.decide()["queued"], false);

    f.config.models.get_mut("fixture").unwrap().model = "fixture-v2".into();
    let changed = f.decide();
    assert_eq!(
        changed["queued"], true,
        "new observer model was ignored: {changed}"
    );
    f.drain().await;
    assert_eq!(f.seen.observed.lock().unwrap().len(), 3);
    assert_eq!(f.decide()["queued"], false);
}

#[tokio::test]
async fn sources_below_the_threshold_wait_and_reaching_it_observes_every_source() {
    let f = Fixture::new(json!({"threshold_tokens":1_000_000,"max_calls":1})).await;
    let first = f.import(&SESSIONS[..1]);
    assert_eq!(first["queued"], false, "{first}");
    assert_eq!(first["reason"], "below threshold");
    let pending = first["unobserved_tokens"].as_u64().unwrap();
    assert!(pending > 0, "{first}");
    f.drain().await;
    assert!(f.seen.observed.lock().unwrap().is_empty());
    assert!(f.jobs("observer_notes").is_empty());
    // The threshold now sits just above the first source: still waiting.
    let policy = f.set_policy(json!({"threshold_tokens":pending + 1,"max_calls":1}));
    assert_eq!(policy["automatic_model_calls"], true, "{policy}");
    assert_eq!(f.decide()["reason"], "below threshold");
    // More sources cross it: one budget-limited round queues, and each round
    // that stops on its budget chains the next until every source is observed.
    let crossed = f.import(&SESSIONS[1..]);
    assert_eq!(crossed["queued"], true, "{crossed}");
    assert_eq!(crossed["kind"], "observer_notes");
    assert!(crossed["unobserved_tokens"].as_u64().unwrap() > pending);
    f.drain().await;
    let prefix = format!("connector:{}:", f.connector);
    assert_eq!(
        f.noted_sources(),
        ["s1", "s2", "s3"].map(|id| format!("{prefix}{id}"))
    );
    // One model call per source; the rounds were automatic and each stayed
    // within its one-call budget.
    assert_eq!(f.seen.observed.lock().unwrap().len(), 3);
    let rounds = f.jobs("observer_notes");
    assert_eq!(rounds.len(), 3, "{rounds:?}");
    for round in &rounds {
        assert_eq!(round["state"], "complete", "{round}");
        assert_eq!(round["payload"]["auto"]["connector_id"], json!(f.connector));
        assert_eq!(round["payload"]["include"], json!([prefix]));
        assert_eq!(round["payload"]["max_calls"], 1);
    }
    assert_eq!(rounds[2]["result"]["complete"], true);
    // Nothing left: no further round, and no model call.
    let settled = f.decide();
    assert_eq!(settled["queued"], false, "{settled}");
    assert_eq!(settled["reason"], "observed");
    assert_eq!(settled["unobserved_tokens"], 0);
    f.drain().await;
    assert_eq!(f.jobs("observer_notes").len(), 3);
    assert_eq!(f.seen.observed.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn sources_arriving_during_a_round_are_observed_once_it_ends() {
    // Mastra 056427cda: activation could leave the newest messages
    // unobserved above the threshold. Here the decision after a round
    // re-counts every unobserved source, so such a tail queues at once.
    let f = Fixture::new(json!({"threshold_tokens":1,"max_calls":1})).await;
    let first = f.import(&SESSIONS[..1]);
    assert_eq!(first["queued"], true, "{first}");
    let during = f.import(&SESSIONS[1..]);
    assert_eq!(during["queued"], false, "{during}");
    assert_eq!(during["reason"], "round in progress");
    f.drain().await;
    let prefix = format!("connector:{}:", f.connector);
    assert_eq!(
        f.noted_sources(),
        ["s1", "s2", "s3"].map(|id| format!("{prefix}{id}"))
    );
    let settled = f.decide();
    assert_eq!(settled["reason"], "observed", "{settled}");
    assert_eq!(settled["unobserved_tokens"], 0);
}

#[tokio::test]
async fn rounds_are_capped_per_generation_and_the_next_import_resumes() {
    let f = Fixture::new(json!({"threshold_tokens":1,"max_calls":1,"max_rounds":1})).await;
    let queued = f.import(&SESSIONS);
    assert_eq!(queued["queued"], true, "{queued}");
    f.drain().await;
    assert_eq!(f.jobs("observer_notes").len(), 1);
    assert_eq!(f.noted_sources().len(), 1);
    let capped = f.decide();
    assert_eq!(capped["queued"], false, "{capped}");
    assert_eq!(
        capped["reason"],
        "observation rounds exhausted for this generation"
    );
    // A new import is a new generation with its own rounds.
    let resumed = f.import(&[("s4", "2023/06/10 (Sat) 08:00", "I started learning Rust.")]);
    assert_eq!(resumed["queued"], true, "{resumed}");
    assert!(resumed["generation"].as_i64() > queued["generation"].as_i64());
    f.drain().await;
    assert_eq!(f.noted_sources().len(), 2);
}

#[tokio::test]
async fn observe_policies_are_validated_and_rounds_are_marked_only_by_the_service() {
    let f = Fixture::new(json!({})).await;
    for (policy, message) in [
        (json!("yes"), "observe must be an object"),
        (
            json!({"threshold":10}),
            "Unknown observe policy field threshold",
        ),
        (json!({"threshold_tokens":0}), "must be positive"),
        (json!({"max_rounds":0}), "must be positive"),
        (
            json!({"reflect":"yes"}),
            "`reflect` is not supported by Graphicle",
        ),
        (json!({"max_calls":99}), "Invalid max_calls"),
        (json!({"max_notes":1000}), "Invalid max_notes"),
    ] {
        let error = f
            .store
            .connector_observe(
                &f.workspace,
                &f.key,
                &json!({"repository_id":f.repo,"connector_id":f.connector,"observe":policy}),
            )
            .unwrap_err();
        assert!(
            error.message.contains(message),
            "{policy}: {}",
            error.message
        );
    }
    let status = || {
        f.store
            .project_status(&f.workspace, &f.key, &json!({"repository_id":f.repo}))
            .unwrap()["automatic_model_calls"]
            .clone()
    };
    assert_eq!(status(), true);
    let cleared = f.set_policy(Value::Null);
    assert_eq!(cleared["automatic_model_calls"], false, "{cleared}");
    assert_eq!(status(), false);
    assert!(
        f.store
            .auto_observe(&f.config, Some(&f.connector))
            .unwrap()
            .is_empty()
    );
    for kind in ["observer_notes", "basic"] {
        let error = f
            .store
            .enqueue(
                &f.workspace,
                &f.key,
                &json!({"kind":kind,"repository_id":f.repo,"include":["connector:"],"question":"q","auto":{"connector_id":f.connector,"generation":1}}),
                &f.config,
            )
            .unwrap_err();
        assert_eq!(error.message, "auto is set by the service", "{kind}");
    }
}
