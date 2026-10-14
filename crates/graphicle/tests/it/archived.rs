//! Unsupported options, option values and job kinds (src/archived.rs) on
//! every surface their entries name. The tests read the guard's own lists, so
//! each entry is exercised here without new code:
//! - an unsupported option turned on is refused with "`<option>` is not
//!   supported by Graphicle", an inert value (absent, null, false, 0 or an old
//!   default) is not refused for it;
//! - an unsupported value is refused the same way;
//! - a job queued with such an option fails at once with that message, and a
//!   stored connector policy that turns one on stops automatic observation
//!   with it as the reason;
//! - a job of an unsupported kind is never leased: it stays queued, job
//!   status names its unit, and a new one is refused.
//!
//! An extension crate can re-run this suite with its extensions installed: no
//! edition leases an unsupported kind, and an extension's own job kinds run
//! beside them.
use graphicle_runtime::{
    api::{self, App},
    archived::{self, Surface},
    jobs::Worker,
    model,
    store::{Store, s},
};
use serde_json::{Value, json};
use tempfile::TempDir;

/// Every surface; `Fixture::send` matches them exhaustively, so a new one
/// fails to compile until it is listed and sent here.
const SURFACES: [Surface; 4] = [
    Surface::Context,
    Surface::Workflow,
    Surface::ObserverContext,
    Surface::ObservePolicy,
];
const SESSION: &str = "Conversation date: 2024/03/02 (Sat) 10:00\n\nuser: The release ships on Friday after the canary.\nassistant: Noted.\n";
const QUESTION: &str = "When does the release ship?";

struct Fixture {
    _dir: TempDir,
    store: Store,
    workspace: String,
    key: String,
    repo: String,
    connector: String,
    config: model::Config,
}
impl Fixture {
    fn new() -> Self {
        crate::edition_setup();
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let ws = store.create_workspace("Unsupported options").unwrap();
        let workspace = s(&ws, "id").to_string();
        let key = s(&ws, "capability").to_string();
        let repo = s(
            &store
                .register(
                    &workspace,
                    &key,
                    &json!({"project_key":"archived","name":"Unsupported options"}),
                )
                .unwrap(),
            "id",
        )
        .to_string();
        // No request reaches a model: refusals happen before any call.
        let config = serde_json::from_value(json!({"default_model":"fast","models":{
            "fast":{"protocol":"responses","base_url":"http://127.0.0.1:9/v1","model":"fast-fixture","input_limit":16000,"output_limit":4096}
        }}))
        .unwrap();
        let mut f = Self {
            _dir: dir,
            store,
            workspace,
            key,
            repo,
            connector: String::new(),
            config,
        };
        let files = [("session-0001.txt", SESSION)];
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
        let added = f
            .command(
                "connector.add",
                json!({"kind":"jsonl","name":"Chat","config":{"acl":{"workspace_readers":true,"credentials":[]}}}),
            )
            .unwrap();
        f.connector = s(&added, "id").to_string();
        f
    }
    fn args(&self, mut args: Value) -> Value {
        args["repository_id"] = json!(self.repo);
        args
    }
    /// One MCP `tools/call` of `name` over this repository.
    fn mcp_call(&self, name: &str, arguments: Value) -> Value {
        let app = App::new(
            self.store.clone(),
            self.config.clone(),
            vec!["localhost".into()],
        )
        .unwrap();
        api::mcp(
            &app,
            &self.workspace,
            &self.key,
            &self.repo,
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":name,"arguments":arguments}}),
        )
        .unwrap()
    }
    fn command(&self, operation: &str, args: Value) -> graphicle_runtime::Result<Value> {
        let app = App::new(
            self.store.clone(),
            self.config.clone(),
            vec!["localhost".into()],
        )
        .unwrap();
        api::command(
            &app,
            &self.workspace,
            &self.key,
            operation,
            &self.args(args),
        )
    }
    /// `fields` sent where callers send options on `surface`.
    fn send(&self, surface: Surface, fields: &Value) -> graphicle_runtime::Result<Value> {
        let with = |mut base: Value| {
            for (name, value) in fields.as_object().unwrap() {
                base[name] = value.clone();
            }
            base
        };
        match surface {
            Surface::Context => self.command("context", with(json!({"question":QUESTION}))),
            Surface::Workflow => self.store.enqueue(
                &self.workspace,
                &self.key,
                &self.args(with(json!({"kind":"basic","question":QUESTION}))),
                &self.config,
            ),
            Surface::ObserverContext => {
                self.command("observer.context", with(json!({"question":QUESTION})))
            }
            Surface::ObservePolicy => self.command(
                "connector.observe",
                json!({"connector_id":self.connector,"observe":with(json!({}))}),
            ),
        }
    }
    fn principal(&self) -> String {
        graphicle_memory::text::sha256(self.key.as_bytes())
    }
    /// A job row as an earlier release could have queued it.
    fn queued(&self, id: &str, kind: &str, payload: &Value, priority: i64) {
        let stamp = graphicle_runtime::now();
        self.store.connect().unwrap().execute(
            "INSERT INTO am_jobs(id,repository_id,kind,input_hash,payload,principal_hash,state,priority,max_attempts,not_before,created_at,updated_at) VALUES(?,?,?,?,?,?,'queued',?,3,?,?,?)",
            rusqlite::params![id, self.repo, kind, id, payload.to_string(), self.principal(), priority, stamp, stamp, stamp],
        )
        .unwrap();
    }
    fn status(&self, id: &str) -> Value {
        self.store
            .job_status(&self.workspace, &self.key, &self.args(json!({"job_id":id})))
            .unwrap()
    }
}

/// A value that turns `entry` on: `true`, or for an option whose old default
/// was `true` (and so is inert) a string.
fn turned_on(entry: &archived::ArchivedOption) -> Value {
    [json!(true), json!("on")]
        .into_iter()
        .find(|value| !(entry.inert)(value))
        .unwrap()
}

/// Values that may be inert for an entry: the ones that are must pass.
fn maybe_inert() -> [Value; 4] {
    [Value::Null, json!(false), json!(0), json!(true)]
}

fn refused_as_archived(result: graphicle_runtime::Result<Value>) -> Option<String> {
    result
        .err()
        .map(|e| e.message)
        .filter(|m| m.contains(" is not supported by Graphicle"))
}

#[test]
fn every_surface_serves_a_request_that_names_no_archived_option() {
    let f = Fixture::new();
    for surface in SURFACES {
        if let Err(error) = f.send(surface, &json!({})) {
            panic!("{surface:?}: {}", error.message);
        }
    }
}

#[tokio::test]
async fn archived_options_are_refused_where_callers_sent_them_and_inert_values_pass() {
    let f = Fixture::new();
    let worker = Worker::new(f.store.clone(), f.config.clone()).unwrap();
    for entry in archived::OPTIONS {
        let message = entry.message();
        for surface in entry.surfaces.iter().copied() {
            let on = json!({ entry.option: turned_on(entry) });
            let error = f.send(surface, &on).unwrap_err();
            assert_eq!(
                error.status.as_u16(),
                400,
                "{} on {surface:?}",
                entry.option
            );
            assert_eq!(error.message, message, "{surface:?}");
            for value in maybe_inert() {
                if !(entry.inert)(&value) {
                    continue;
                }
                let mut fields = json!({ entry.option: value });
                for dependent in entry.dependents {
                    fields[*dependent] = json!(true);
                }
                assert_eq!(
                    refused_as_archived(f.send(surface, &fields)),
                    None,
                    "{fields} on {surface:?}"
                );
            }
        }
        // Surfaces it was never sent on ignore it as before.
        for surface in SURFACES
            .iter()
            .copied()
            .filter(|s| !entry.surfaces.contains(s))
        {
            assert_eq!(
                refused_as_archived(f.send(surface, &json!({ entry.option: turned_on(entry) }))),
                None,
                "{} on {surface:?}",
                entry.option
            );
        }
        if entry.surfaces.contains(&Surface::Workflow) {
            // Queued before the archive: it fails at once, not silently and
            // not on retry.
            let id = format!("queued-{}", entry.option);
            f.queued(
                &id,
                "basic",
                &json!({"kind":"basic","question":QUESTION, entry.option: turned_on(entry)}),
                100,
            );
            assert!(worker.once().await.unwrap());
            let job = f.status(&id);
            assert_eq!(job["state"], "failed", "{job}");
            assert_eq!(job["error"], message.as_str(), "{job}");
        }
        if entry.surfaces.contains(&Surface::ObservePolicy) {
            // A policy stored before the archive stops automatic observation
            // and says why.
            f.store
                .connect()
                .unwrap()
                .execute(
                    "UPDATE am_connectors SET config=json_set(config,'$.observe',json(?)) WHERE id=?",
                    rusqlite::params![
                        json!({ entry.option: turned_on(entry) }).to_string(),
                        f.connector
                    ],
                )
                .unwrap();
            let decisions = f.store.auto_observe(&f.config, Some(&f.connector)).unwrap();
            assert!(
                decisions
                    .iter()
                    .any(|d| d["queued"] == false && d["reason"] == message.as_str()),
                "{decisions:?}"
            );
            f.command(
                "connector.observe",
                json!({"connector_id":f.connector,"observe":null}),
            )
            .unwrap();
        }
    }
}

/// MCP `recall_project` checks its arguments against the published schema,
/// which no longer lists archived `context` options: an inert one is left out
/// before that check, any other value is refused with its unit.
#[test]
fn mcp_recall_project_leaves_out_inert_archived_options_and_refuses_the_rest() {
    let f = Fixture::new();
    let plain = f.mcp_call("recall_project", json!({"question":QUESTION}));
    assert_eq!(plain["result"]["isError"], false, "{plain}");
    let mut checked = 0;
    for entry in archived::OPTIONS
        .iter()
        .filter(|e| e.surfaces.contains(&Surface::Context))
    {
        checked += 1;
        for value in maybe_inert() {
            if !(entry.inert)(&value) {
                continue;
            }
            let reply = f.mcp_call(
                "recall_project",
                json!({"question":QUESTION, entry.option: value}),
            );
            assert_eq!(
                reply["result"]["isError"], false,
                "{}: {reply}",
                entry.option
            );
        }
        let reply = f.mcp_call(
            "recall_project",
            json!({"question":QUESTION, entry.option: turned_on(entry)}),
        );
        assert_eq!(
            reply["error"]["message"],
            entry.message().as_str(),
            "{}: {reply}",
            entry.option
        );
    }
    // A field that was never published is still refused by the schema.
    let reply = f.mcp_call(
        "recall_project",
        json!({"question":QUESTION,"never_an_option":true}),
    );
    assert!(
        reply["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("published contract")),
        "{reply}"
    );
    assert!(checked > 0, "no archived context option to check");
}

#[test]
fn archived_values_are_refused_where_their_option_is_read() {
    let f = Fixture::new();
    for entry in archived::VALUES {
        for surface in entry.surfaces.iter().copied() {
            let error = f
                .send(surface, &json!({ entry.option: entry.value }))
                .unwrap_err();
            assert_eq!(error.status.as_u16(), 400, "{surface:?}");
            assert_eq!(error.message, entry.message(), "{surface:?}");
        }
    }
}

#[tokio::test]
async fn archived_job_kinds_stay_queued_report_where_they_went_and_are_refused() {
    let f = Fixture::new();
    for (i, entry) in archived::KINDS.iter().enumerate() {
        // Ahead of every other job in the queue.
        f.queued(&format!("archived-{i}"), entry.kind, &json!({}), 100);
    }
    // the commercial edition also queues its own kind, the semantic overlay. Core has
    // neither the kind nor the operation.
    let pro = graphicle_runtime::core_capabilities::capabilities()["edition"] == "pro";
    let overlay = f.command("overlay.enqueue", json!({}));
    assert_eq!(overlay.is_ok(), pro, "{overlay:?}");
    let worker = Worker::new(f.store.clone(), f.config.clone()).unwrap();
    // The sync's graph projection still runs (and the commercial edition's overlay); nothing
    // else is runnable.
    assert!(worker.once().await.unwrap(), "the projection runs");
    while worker.once().await.unwrap() {}
    let jobs = f
        .store
        .job_status(&f.workspace, &f.key, &f.args(json!({})))
        .unwrap();
    let live: Vec<&Value> = jobs["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|j| j["kind"] == "project" || j["kind"] == "semantic_overlay")
        .collect();
    assert_eq!(live.len(), if pro { 2 } else { 1 }, "{jobs}");
    for job in live {
        assert_eq!(job["state"], "complete", "{job}");
        assert!(job.get("archived").is_none(), "{job}");
    }
    for (i, entry) in archived::KINDS.iter().enumerate() {
        let job = f.status(&format!("archived-{i}"));
        assert_eq!(job["state"], "queued", "{job}");
        assert_eq!(job["attempt"], 0, "{job}");
        assert!(job["error"].is_null(), "{job}");
        assert_eq!(job["waiting_for"], entry.waiting().as_str(), "{job}");
        assert_eq!(job["archived"], format!("`{}`", entry.unit).as_str());
        assert_eq!(job["archived"], entry.listing().as_str());
        let error = f
            .store
            .enqueue(
                &f.workspace,
                &f.key,
                &f.args(json!({"kind":entry.kind})),
                &f.config,
            )
            .unwrap_err();
        assert_eq!(error.message, entry.message());
        assert_eq!(
            graphicle_runtime::jobs::permission(entry.kind)
                .unwrap_err()
                .message,
            entry.message()
        );
        // The owner can still cancel it.
        f.store
            .cancel_job(
                &f.workspace,
                &f.key,
                &f.args(json!({"job_id":format!("archived-{i}")})),
            )
            .unwrap();
        assert_eq!(f.status(&format!("archived-{i}"))["state"], "cancelled");
    }
}
