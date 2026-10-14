//! A code graph that is always there and always used.
//!
//! The projection drain runs in Core (`jobs::drain_projection`):
//! `graphicle sync` builds the graph in the foreground (or `--background`,
//! or later with `--no-graph`), `graphicle mcp` drains at start, and `ask`
//! and `run --wait` before they queue; leases keep two drains, or a drain and
//! a worker, from doing the work twice. While a new head set waits for its
//! projection the arm reads the latest episode and cites only unchanged
//! files (the lag fallback). `basic` reads the code graph and reviewed memory
//! on a project with code symbols and
//! answers a conversation-only project as before. MCP `recall_project`
//! honours the `graph_retrieval`, `memory_retrieval` and `include` switches.
use axum::{Json, Router, extract::State, routing::post};
use graphicle_runtime::{
    api::{self, App},
    graph_projection, jobs, local, model,
    store::{Store, s},
};
use serde_json::{Value, json};
use std::{
    path::Path,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tempfile::TempDir;

const A_V1: &str = "def alpha():\n    return 1\n\ndef beta():\n    return 2\n";
const A_V2: &str = "def alpha():\n    return 10\n\ndef beta():\n    return 2\n";
const B: &str = "from a import alpha, beta\n\ndef run():\n    return alpha() + beta()\n";

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
        let ws = store.create_workspace("Graph always").unwrap();
        let workspace = s(&ws, "id").to_string();
        let key = s(&ws, "capability").to_string();
        let repo = s(
            &store
                .register(
                    &workspace,
                    &key,
                    &json!({"project_key":"always","name":"Always"}),
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
        f.sync(files);
        f
    }
    fn args(&self, mut args: Value) -> Value {
        args["repository_id"] = json!(self.repo);
        args
    }
    fn manifest(files: &[(&str, &str)]) -> Vec<Value> {
        files
            .iter()
            .map(|(name, text)| json!({"path":name,"sha256":graphicle_memory::text::sha256(text.as_bytes()),"bytes":text.len()}))
            .collect()
    }
    fn prepare(&self, files: &[(&str, &str)]) -> Value {
        self.prepare_as(&self.key, files)
    }
    fn prepare_as(&self, key: &str, files: &[(&str, &str)]) -> Value {
        let state = self
            .store
            .project_status(&self.workspace, key, &self.args(json!({})))
            .unwrap();
        self.store
            .sync_prepare(
                &self.workspace,
                key,
                &self.args(json!({"expected_snapshot":state["repository"]["snapshot"],"manifest":Self::manifest(files)})),
            )
            .unwrap()
    }
    /// Sync `files` as a client does; the projection is only queued.
    fn sync(&self, files: &[(&str, &str)]) {
        self.sync_as(&self.key, files);
    }
    fn sync_as(&self, key: &str, files: &[(&str, &str)]) {
        let begin = self.prepare_as(key, files);
        let upload: Vec<Value> = files
            .iter()
            .map(|(name, text)| json!({"path":name,"content":text}))
            .collect();
        self.store
            .sync_upload(
                &self.workspace,
                key,
                &self.args(json!({"sync_id":begin["sync_id"],"files":upload})),
            )
            .unwrap();
        self.store
            .sync_commit(
                &self.workspace,
                key,
                &self.args(json!({"sync_id":begin["sync_id"]})),
            )
            .unwrap();
    }
    fn session(&self) -> local::Session {
        local::Session {
            workspace: self.workspace.clone(),
            capability: self.key.clone(),
            project: None,
        }
    }
    /// One column of a job's row.
    fn job_field(&self, id: &str, column: &str) -> Value {
        self.store
            .connect()
            .unwrap()
            .query_row(
                &format!("SELECT {column} FROM am_jobs WHERE id=?"),
                [id],
                |r| {
                    Ok(match r.get_ref(0)? {
                        rusqlite::types::ValueRef::Null => Value::Null,
                        rusqlite::types::ValueRef::Integer(n) => json!(n),
                        rusqlite::types::ValueRef::Text(t) => {
                            json!(String::from_utf8_lossy(t).into_owned())
                        }
                        other => json!(format!("{other:?}")),
                    })
                },
            )
            .unwrap()
    }
    fn drain(&self) -> Vec<Value> {
        jobs::drain_projection(&self.store, &self.repo).unwrap()
    }
    fn current(&self) -> String {
        self.store
            .read(|db| graph_projection::current_episode(db, &self.repo))
            .unwrap()
    }
    fn episodes(&self) -> usize {
        self.store
            .read(|db| graph_projection::episodes(db, &self.repo))
            .unwrap()
            .len()
    }
    fn projection(&self) -> String {
        let episode = self.current();
        self.store
            .read(|db| jobs::projection_state(db, &self.repo, &episode))
            .unwrap()
            .to_string()
    }
    fn context(&self, question: &str) -> Value {
        self.store
            .project_context(
                &self.workspace,
                &self.key,
                &self.args(json!({"question":question})),
            )
            .unwrap()
    }
    fn note(&self, title: &str, body: &str, dependencies: &[&str]) {
        self.store
            .note(
                &self.workspace,
                &self.key,
                &self.args(json!({"title":title,"body":body,"dependencies":dependencies})),
            )
            .unwrap();
    }
    fn project_job(&self) -> String {
        let row: String = self
            .store
            .connect()
            .unwrap()
            .query_row(
                "SELECT id FROM am_jobs WHERE repository_id=? AND kind='project' ORDER BY created_at DESC LIMIT 1",
                [&self.repo],
                |r| r.get(0),
            )
            .unwrap();
        row
    }
}

/// The `code_graph` facts of the packet's evidence windows.
fn cited(packet: &Value) -> Vec<Value> {
    packet["evidence"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|window| window.get("code_graph").cloned())
        .collect()
}

fn kinds(ran: &[Value]) -> Vec<&str> {
    ran.iter().map(|job| s(job, "kind")).collect()
}

/// First case: edit one file and sync. Before the
/// projection runs, nodes in unchanged files are still fused, the changed
/// file's nodes are not cited, and the packet says `episode_lag: 1`.
#[test]
fn while_a_sync_waits_for_its_projection_the_arm_reads_the_latest_episode() {
    let f = Fixture::new(&[("a.py", A_V1), ("b.py", B)]);
    assert_eq!(kinds(&f.drain()), ["project", "link"]);
    let recorded = f.current();
    f.sync(&[("a.py", A_V2), ("b.py", B)]);
    assert_eq!(f.projection(), "queued");

    // `beta` sits in the changed a.py: it leads the walk to its caller in
    // b.py, which is cited, and is never cited itself.
    let packet = f.context("who calls beta");
    let arm = &packet["code_graph"];
    assert_eq!(arm["status"], "fused", "{arm}");
    assert_eq!(arm["episode"], recorded.as_str(), "{arm}");
    assert_eq!(arm["episode_lag"], 1, "{arm}");
    assert_eq!(arm["current_episode"], f.current().as_str(), "{arm}");
    assert_eq!(arm["projection"], "queued", "{arm}");
    assert!(arm["withheld_changed"].as_u64().unwrap_or(0) >= 1, "{arm}");
    assert_eq!(packet["mechanisms"]["code_graph_searched"], true);
    let facts = cited(&packet);
    assert!(
        facts.iter().any(|fact| fact["label"] == "run()"),
        "{facts:?}"
    );
    assert!(
        facts.iter().all(|fact| fact["relpath"] == "b.py"),
        "a changed file's node was cited: {facts:?}"
    );
    // A question about the changed file's own symbols cites none of them.
    let alpha = f.context("where is alpha defined");
    assert!(
        cited(&alpha).iter().all(|fact| fact["relpath"] != "a.py"),
        "{}",
        alpha["evidence"]
    );
    assert_eq!(alpha["code_graph"]["episode_lag"], 1);

    // Once drained, the arm reads the new head set with no lag.
    assert_eq!(kinds(&f.drain()), ["project", "link"]);
    let current = f.context("who calls beta");
    assert_eq!(current["code_graph"]["status"], "fused");
    assert_eq!(current["code_graph"]["episode"], f.current().as_str());
    assert_eq!(current["code_graph"]["episode_lag"], 0);
    assert!(current["code_graph"].get("current_episode").is_none());
    // An episode recorded without its source manifest (older stores) cannot
    // be checked per file: the arm is skipped and says so.
    let latest = f.current();
    f.store
        .connect()
        .unwrap()
        .execute(
            "UPDATE gf_episodes SET attributes=json_remove(attributes,'$.source_heads') WHERE repository_id=?",
            [&f.repo],
        )
        .unwrap();
    f.sync(&[("a.py", A_V1), ("b.py", "def run():\n    return 0\n")]);
    let unknown = f.context("who calls beta");
    let arm = &unknown["code_graph"];
    assert_eq!(arm["status"], "no_episode", "{arm}");
    assert_eq!(arm["projection"], "queued", "{arm}");
    assert_eq!(arm["latest_episode"]["episode"], latest.as_str(), "{arm}");
    assert!(arm["episode_lag"].is_null(), "{arm}");
}

const CALLER_V1: &str = "from helpers import helper\n\n\ndef alpha():\n    return helper()\n";
const CALLER_V2: &str = "def alpha():\n    return 2\n";
const HELPERS: &str = "def helper():\n    return 1\n";
const USER: &str = "from caller import alpha\n\n\ndef run():\n    return alpha()\n";

/// The lag fallback follows only edges read from unchanged files.
/// `alpha` stops calling `helper` in the changed caller.py: while the new
/// head set waits for its projection, the latest episode still holds that
/// call, read from caller.py, so `helper()` is not cited through it; `run()`
/// still calls `alpha`, read from the unchanged user.py, and stays cited.
#[test]
fn the_lag_fallback_follows_only_edges_read_from_unchanged_files() {
    let question = "what does alpha call";
    let f = Fixture::new(&[
        ("caller.py", CALLER_V1),
        ("helpers.py", HELPERS),
        ("user.py", USER),
    ]);
    f.drain();
    // On the current episode the call is cited, through `alpha`.
    let current = cited(&f.context(question));
    let helper = current
        .iter()
        .find(|fact| fact["label"] == "helper()")
        .unwrap_or_else(|| panic!("the call is not cited on the current episode: {current:?}"));
    assert_eq!(helper["via"]["relation"], "calls", "{helper}");

    f.sync(&[
        ("caller.py", CALLER_V2),
        ("helpers.py", HELPERS),
        ("user.py", USER),
    ]);
    let packet = f.context(question);
    let arm = &packet["code_graph"];
    assert_eq!(arm["status"], "fused", "{arm}");
    assert_eq!(arm["episode_lag"], 1, "{arm}");
    assert!(
        arm["withheld_stale_edges"].as_u64().unwrap_or(0) >= 1,
        "{arm}"
    );
    let facts = cited(&packet);
    assert!(
        facts.iter().all(|fact| fact["label"] != "helper()"),
        "a call the changed file no longer makes was cited: {facts:?}"
    );
    let run = facts
        .iter()
        .find(|fact| fact["label"] == "run()")
        .unwrap_or_else(|| panic!("the unchanged caller is not cited: {facts:?}"));
    assert_eq!(run["relpath"], "user.py");
    assert_eq!(run["via"]["relation"], "calls", "{run}");
    assert!(
        facts.iter().all(|fact| fact["relpath"] != "caller.py"),
        "{facts:?}"
    );

    // Projected, the episode has no such call either.
    f.drain();
    let projected = f.context(question);
    assert_eq!(projected["code_graph"]["episode_lag"], 0);
    assert!(
        projected["code_graph"]
            .get("withheld_stale_edges")
            .is_none()
    );
    assert!(
        cited(&projected)
            .iter()
            .all(|fact| fact["label"] != "helper()"),
        "{}",
        projected["evidence"]
    );
}

/// Second case: draining twice, or beside a worker (a
/// running `graphicle serve`), does the work once.
#[test]
fn draining_twice_or_beside_a_worker_does_the_work_once() {
    let f = Fixture::new(&[("a.py", A_V1), ("b.py", B)]);
    assert_eq!(kinds(&f.drain()), ["project", "link"]);
    assert!(f.drain().is_empty(), "nothing is left to run");
    assert_eq!(f.episodes(), 1);

    // A worker leased the next projection first: the drain leaves it.
    f.sync(&[("a.py", A_V2), ("b.py", B)]);
    let held = f
        .store
        .lease_job_of_kind(&f.repo, "project")
        .unwrap()
        .unwrap();
    assert_eq!(f.projection(), "running");
    assert!(
        !kinds(&f.drain()).contains(&"project"),
        "the drain ran a job a worker holds"
    );
    let result = jobs::run_projection(&f.store, &held);
    f.store.finish_job(&held, result).unwrap();
    assert_eq!(f.episodes(), 2);
    assert_eq!(kinds(&f.drain()), ["link"]);

    // Two drains at once run the projection once between them.
    f.sync(&[
        ("a.py", A_V1),
        ("b.py", B),
        ("c.py", "def gamma():\n    return 3\n"),
    ]);
    let (one, other) = std::thread::scope(|scope| {
        let one = scope.spawn(|| jobs::drain_projection(&f.store, &f.repo).unwrap());
        let other = scope.spawn(|| jobs::drain_projection(&f.store, &f.repo).unwrap());
        (one.join().unwrap(), other.join().unwrap())
    });
    let projections = kinds(&one)
        .into_iter()
        .chain(kinds(&other))
        .filter(|kind| *kind == "project")
        .count();
    assert_eq!(projections, 1, "{one:?} {other:?}");
    assert_eq!(f.episodes(), 3);
    let attempts: i64 = f
        .store
        .connect()
        .unwrap()
        .query_row(
            "SELECT max(attempt) FROM am_jobs WHERE repository_id=? AND kind='project'",
            [&f.repo],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(attempts, 1, "every projection ran once");
}

/// The lease stamp `seconds` from now, in the store's format.
fn lease_in(seconds: f64) -> String {
    (chrono::Utc::now() + chrono::Duration::milliseconds((seconds * 1000.0) as i64))
        .to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

/// A foreground build waits for a projection another process
/// leased and takes it over once that lease expires (here the process is
/// gone); a drain that is told to stop starts no job.
#[test]
fn a_foreground_build_takes_over_a_projection_whose_lease_expired() {
    let f = Fixture::new(&[("a.py", A_V1), ("b.py", B)]);
    let stop = std::sync::atomic::AtomicBool::new(true);
    assert!(
        jobs::drain_projection_until(&f.store, &f.repo, Some(&stop))
            .unwrap()
            .is_empty()
    );
    assert_eq!(f.projection(), "queued");

    let held = f
        .store
        .lease_job_of_kind(&f.repo, "project")
        .unwrap()
        .unwrap();
    // The holder's lease ends 0.4 s from now, before the build's third
    // drain (its drains are at least 250 ms apart), and the build drains
    // again a projection that one of its first two drains leaves queued.
    // So the takeover does not depend on how fast this machine runs.
    let until = lease_in(0.4);
    f.store
        .connect()
        .unwrap()
        .execute(
            "UPDATE am_jobs SET lease_until=? WHERE id=?",
            [until.clone(), held.id.clone()],
        )
        .unwrap();
    let report = local::build_graph(&f.store, &f.session(), &f.repo, false).unwrap();
    assert_eq!(report["status"], "built", "{report}");
    // It waited for the lease to end, judged by the store's own clock.
    assert!(
        graphicle_runtime::now() > until,
        "built before the lease ended"
    );
    assert_eq!(f.job_field(&held.id, "state"), "complete");
    assert_eq!(
        f.job_field(&held.id, "attempt"),
        2,
        "the expired lease counted"
    );
    // The holder's late finish changes nothing.
    f.store
        .finish_job(
            &held,
            Err(graphicle_runtime::Error::bad("the holder was gone")),
        )
        .unwrap();
    assert_eq!(f.job_field(&held.id, "state"), "complete");
}

/// A running `project` or `link` job renews its lease while it runs,
/// so a projection longer than the 240 s lease keeps it; a job that was
/// cancelled meanwhile is left alone.
#[test]
fn a_running_projection_renews_its_lease() {
    let f = Fixture::new(&[("a.py", A_V1), ("b.py", B)]);
    let held = f
        .store
        .lease_job_of_kind(&f.repo, "project")
        .unwrap()
        .unwrap();
    let soon = lease_in(2.0);
    let set_lease = |until: &str| {
        f.store
            .connect()
            .unwrap()
            .execute(
                "UPDATE am_jobs SET lease_until=? WHERE id=?",
                [until, held.id.as_str()],
            )
            .unwrap();
    };
    set_lease(&soon);
    let value = jobs::with_lease_heartbeat(&f.store, &held, Duration::from_millis(100), || {
        std::thread::sleep(Duration::from_millis(600));
        7
    });
    assert_eq!(value, 7);
    let renewed = f.job_field(&held.id, "lease_until");
    assert!(
        renewed.as_str().unwrap() > lease_in(200.0).as_str(),
        "{renewed} after {soon}"
    );
    // A cancelled job keeps no lease.
    f.store
        .cancel_job(&f.workspace, &f.key, &f.args(json!({"job_id":held.id})))
        .unwrap();
    set_lease(&soon);
    jobs::with_lease_heartbeat(&f.store, &held, Duration::from_millis(100), || {
        std::thread::sleep(Duration::from_millis(400));
    });
    assert_eq!(f.job_field(&held.id, "lease_until"), soon.as_str());
    assert_eq!(jobs::LEASE_HEARTBEAT, Duration::from_secs(60));
}

/// SQLite's busy error is recognised, and a foreground build behind
/// another process's long write waits for it instead of reporting the
/// graph as failed.
#[test]
fn a_foreground_build_waits_while_another_process_holds_the_store() {
    let f = Fixture::new(&[("a.py", A_V1), ("b.py", B)]);
    // A real busy error, as the drain sees it.
    let holder = f.store.connect().unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();
    let other = f.store.connect().unwrap();
    other.busy_timeout(Duration::ZERO).unwrap();
    let error = graphicle_runtime::Error::from(other.execute_batch("BEGIN IMMEDIATE").unwrap_err());
    assert!(jobs::busy(&error), "{}", error.message);
    assert!(!jobs::busy(&graphicle_runtime::Error::bad(
        "Graph projection failed: parser crashed"
    )));
    holder.execute_batch("ROLLBACK").unwrap();

    // Another process holds the write lock past the busy timeout (10 s).
    let (locked, wait) = std::sync::mpsc::channel();
    let store = f.store.clone();
    let writer = std::thread::spawn(move || {
        let db = store.connect().unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        locked.send(()).unwrap();
        std::thread::sleep(Duration::from_secs(12));
        db.execute_batch("COMMIT").unwrap();
    });
    wait.recv().unwrap();
    let started = Instant::now();
    let report = local::build_graph(&f.store, &f.session(), &f.repo, false).unwrap();
    writer.join().unwrap();
    assert_eq!(report["status"], "built", "{report}");
    assert!(started.elapsed() >= Duration::from_secs(10));
    assert_eq!(f.episodes(), 1);
}

/// A projection that failed for good is tried again by the next sync of the
/// same tree, and the skip reason follows a retry of the job, which has an
/// input hash of its own.
#[test]
fn a_sync_retries_a_failed_projection_and_the_skip_reason_follows_a_retry() {
    let f = Fixture::new(&[("a.py", A_V1), ("b.py", B)]);
    let original = f.project_job();
    let fail = |id: &str| {
        f.store
            .connect()
            .unwrap()
            .execute(
                "UPDATE am_jobs SET state='failed',attempt=max_attempts,error='parser crashed' WHERE id=?",
                [id],
            )
            .unwrap();
    };
    fail(&original);
    let skipped = f.context("where is beta defined");
    assert_eq!(skipped["code_graph"]["status"], "no_episode");
    assert_eq!(skipped["code_graph"]["projection"], "failed");

    // `workflow.retry` queues a new job for the same head set.
    let config: model::Config = serde_json::from_value(json!({"default_model":"m","models":{
        "m":{"protocol":"responses","base_url":"http://127.0.0.1:9/v1","model":"m","input_limit":16000,"output_limit":2048}}}))
    .unwrap();
    let retry = f
        .store
        .retry_job(
            &f.workspace,
            &f.key,
            &f.args(json!({"job_id":original})),
            &config,
        )
        .unwrap();
    assert_ne!(retry["id"], original.as_str());
    assert_eq!(f.projection(), "queued");
    assert_eq!(
        f.context("where is beta defined")["code_graph"]["projection"],
        "queued"
    );
    fail(s(&retry, "id"));
    assert_eq!(f.projection(), "failed");

    // A sync of the same tree gives the head set's own job a fresh start,
    // and the drain records the episode.
    let replayed = f.prepare(&[("a.py", A_V1), ("b.py", B)]);
    assert_eq!(replayed["replayed"], true, "{replayed}");
    assert_eq!(replayed["projection_job"]["id"], original.as_str());
    assert_eq!(replayed["projection_job"]["state"], "queued");
    assert_eq!(f.projection(), "queued");
    let ran = f.drain();
    assert_eq!(ran[0]["job"], original.as_str(), "{ran:?}");
    assert_eq!(ran[0]["ok"], true);
    assert_eq!(f.episodes(), 1);
    assert_eq!(
        f.context("where is beta defined")["code_graph"]["status"],
        "fused"
    );
}

/// A sync of the same tree restarts a cancelled
/// projection with a fresh budget, and lets one that waits to retry run at
/// once.
#[test]
fn a_sync_restarts_a_cancelled_projection_and_runs_a_waiting_retry_now() {
    let files = [("a.py", A_V1), ("b.py", B)];
    let f = Fixture::new(&files);
    let job = f.project_job();
    f.store
        .cancel_job(&f.workspace, &f.key, &f.args(json!({"job_id":job})))
        .unwrap();
    assert_eq!(f.projection(), "none");
    let replayed = f.prepare(&files);
    assert_eq!(replayed["projection_job"]["id"], job.as_str());
    assert_eq!(replayed["projection_job"]["state"], "queued");
    assert_eq!(f.job_field(&job, "state"), "queued");
    assert_eq!(f.job_field(&job, "attempt"), 0);

    // Waiting to retry, an hour from now: a drain leaves it.
    let later = lease_in(3600.0);
    f.store
        .connect()
        .unwrap()
        .execute(
            "UPDATE am_jobs SET state='retry',attempt=1,not_before=? WHERE id=?",
            [later.as_str(), job.as_str()],
        )
        .unwrap();
    assert!(f.drain().is_empty());
    assert_eq!(f.projection(), "queued");
    // A sync of the same tree lets it run now.
    f.prepare(&files);
    assert!(f.job_field(&job, "not_before").as_str().unwrap() < later.as_str());
    let ran = f.drain();
    assert_eq!(ran[0]["job"], job.as_str(), "{ran:?}");
    assert_eq!(ran[0]["ok"], true);
    assert_eq!(f.job_field(&job, "attempt"), 2);
    assert_eq!(f.episodes(), 1);
}

/// A projection restarted by a sync runs as the syncing key. The
/// key that queued it was revoked (its jobs cancelled) or has expired;
/// leasing the job under that key would cancel it again ("Credential
/// unavailable") and leave the head set without its episode.
#[test]
fn a_projection_restarted_by_another_key_runs_as_that_key() {
    let f = Fixture::new(&[("a.py", A_V1), ("b.py", B)]);
    f.drain();
    let owner = f.job_field(&f.project_job(), "principal_hash");
    let grant = |label: &str| {
        f.store
            .grant(
                &f.workspace,
                &f.key,
                &json!({"role":"collector","label":label}),
            )
            .unwrap()
    };

    // Revoked: the revocation cancels the job its key queued.
    let revoked = grant("revoked");
    let second = [("a.py", A_V2), ("b.py", B)];
    f.sync_as(s(&revoked, "capability"), &second);
    let job = f.project_job();
    assert_ne!(f.job_field(&job, "principal_hash"), owner);
    f.store
        .revoke(&f.workspace, &f.key, s(&revoked, "id"))
        .unwrap();
    assert_eq!(f.job_field(&job, "state"), "cancelled");
    f.prepare(&second);
    assert_eq!(f.job_field(&job, "principal_hash"), owner);
    assert_eq!(kinds(&f.drain()), ["project", "link"]);
    assert_eq!(f.episodes(), 2);

    // Expired while its job waits: the job is still queued, and a sync by
    // another key hands it over.
    let expiring = grant("expiring");
    let third = [
        ("a.py", A_V1),
        ("b.py", B),
        ("c.py", "def gamma():\n    return 3\n"),
    ];
    f.sync_as(s(&expiring, "capability"), &third);
    let job = f.project_job();
    f.store
        .connect()
        .unwrap()
        .execute(
            "UPDATE credentials SET expires_at=? WHERE id=?",
            [lease_in(-1.0).as_str(), s(&expiring, "id")],
        )
        .unwrap();
    assert_eq!(f.job_field(&job, "state"), "queued");
    f.prepare(&third);
    assert_eq!(f.job_field(&job, "principal_hash"), owner);
    assert_eq!(kinds(&f.drain()), ["project", "link"]);
    assert_eq!(f.job_field(&job, "state"), "complete");
    assert_eq!(f.episodes(), 3);
}

/// A projection that finished as a skip (its head set moved on before it
/// ran) runs again when a later sync returns to that head set; it
/// used to stay complete, and that head set never got its episode.
#[test]
fn a_head_set_skipped_once_is_projected_when_a_sync_returns_to_it() {
    let f = Fixture::new(&[("a.py", A_V1), ("b.py", B)]);
    let first = f.current();
    f.sync(&[("a.py", A_V2), ("b.py", B)]);
    let ran = f.drain();
    assert_eq!(kinds(&ran), ["project", "project", "link"], "{ran:?}");
    assert_eq!(f.episodes(), 1, "the first head set's job was a skip");
    f.sync(&[("a.py", A_V1), ("b.py", B)]);
    assert_eq!(f.current(), first);
    assert_eq!(f.projection(), "queued");
    assert_eq!(kinds(&f.drain()), ["project", "link"]);
    assert_eq!(f.episodes(), 2);
    assert_eq!(
        f.context("who calls beta")["code_graph"]["episode"],
        first.as_str()
    );
}

#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<Value>>>);

/// The answer model quotes the first source it was given.
async fn inference(State(seen): State<Seen>, Json(request): Json<Value>) -> Json<Value> {
    let input: Value = serde_json::from_str(request["input"].as_str().unwrap()).unwrap();
    seen.0.lock().unwrap().push(input.clone());
    let source = &input["sources"][0];
    let answer = json!({"answer":"from the first source","abstain":false,
        "evidence":[{"id":source["id"],"quote":source["text"]}],"unresolved":[]});
    Json(
        json!({"status":"completed","model":"fixture","usage":{"input_tokens":150,"output_tokens":80},"output":[{"type":"message","content":[{"type":"output_text","text":answer.to_string()}]}]}),
    )
}

struct Provider {
    config: model::Config,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Provider {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn provider() -> Provider {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/v1/responses", post(inference))
        .with_state(Seen::default());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let config = serde_json::from_value(json!({"default_model":"fast","models":{
        "fast":{"protocol":"responses","base_url":format!("http://{address}/v1"),"model":"fixture","input_limit":16000,"output_limit":4096}
    }}))
    .unwrap();
    Provider { config, task }
}

impl Fixture {
    /// Queue a `basic` answer and run it as a worker would.
    async fn basic(&self, m: &Provider, mut args: Value) -> Value {
        args["kind"] = json!("basic");
        let queued = self
            .store
            .enqueue(&self.workspace, &self.key, &self.args(args), &m.config)
            .unwrap();
        jobs::Worker::new(self.store.clone(), m.config.clone())
            .unwrap()
            .once()
            .await
            .unwrap();
        let done = self
            .store
            .job_status(
                &self.workspace,
                &self.key,
                &self.args(json!({"job_id":queued["id"]})),
            )
            .unwrap();
        assert_eq!(done["state"], "complete", "{done}");
        done["result"]["context"].clone()
    }
}

/// Third case: on a project whose episode holds code
/// symbols, a `basic` answer runs the code arm and reads reviewed memory
/// with `context`'s defaults; an explicit switch still wins.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn basic_reads_the_code_graph_and_reviewed_memory_on_a_code_project() {
    let f = Fixture::new(&[("a.py", A_V1), ("b.py", B)]);
    f.drain();
    f.note(
        "beta is stable",
        "beta() in a.py always returns 2; run() relies on it.",
        &["a.py"],
    );
    let m = provider().await;
    let packet = f
        .basic(&m, json!({"question":"where is beta defined"}))
        .await;
    assert_eq!(
        packet["integration"],
        json!({"default":"integrated","reason":"code_symbols","episode":f.current(),
            "graph_retrieval":true,"memory_retrieval":true,"doc_links":false,"requested":[]}),
        "{}",
        packet["integration"]
    );
    let mechanisms = &packet["mechanisms"];
    assert_eq!(mechanisms["code_graph_searched"], true, "{mechanisms}");
    assert_eq!(packet["code_graph"]["status"], "fused");
    assert_eq!(mechanisms["reviewed_memory_enabled"], true);
    assert_eq!(mechanisms["reviewed_memory_available"], 1);
    // `graph_local` as `context` builds it: beside a current reviewed memory.
    assert_eq!(mechanisms["graph_local_status"], "searched", "{mechanisms}");
    // The arm's windows were selected; the answer's own repacking keeps the
    // windows' bytes only, as before, so the facts stay in the report.
    assert!(
        packet["code_graph"]["cited"].as_u64().unwrap_or(0) >= 1,
        "{}",
        packet["code_graph"]
    );
    assert!(
        packet["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|window| window["id"] == "file:a.py"),
        "{}",
        packet["evidence"]
    );

    let off = f
        .basic(
            &m,
            json!({"question":"where is beta defined","graph_retrieval":false,"memory_retrieval":false}),
        )
        .await;
    assert_eq!(off["code_graph"]["status"], "disabled");
    assert_eq!(off["mechanisms"]["reviewed_memory_enabled"], false);
    assert_eq!(off["mechanisms"]["graph_local_status"], "disabled");
    assert_eq!(off["integration"]["default"], "integrated");
    assert_eq!(
        off["integration"]["requested"],
        json!(["graph_retrieval", "memory_retrieval"])
    );
    assert_eq!(off["code_graph"]["cited"], 0);
}

/// On a conversation-only project, which has an episode once its
/// sync is projected but no code symbol, a `basic` answer's retrieval
/// packet is the one it was before the code graph arm, the switches off.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn basic_on_a_conversation_project_keeps_its_source_only_packet() {
    let f = Fixture::new(&[
        (
            "session-1.txt",
            "Conversation date: 2023/05/20\n\nuser: I adopted a cat named Luna.\nassistant: Congratulations!\n",
        ),
        (
            "session-2.txt",
            "Conversation date: 2023/05/27\n\nuser: Luna had her vet visit today.\nassistant: Glad she is well.\n",
        ),
        (
            "notes.md",
            "# Notes\n\nLuna is a grey cat. Her vet is on Elm Street.\n",
        ),
    ]);
    f.drain();
    assert_eq!(f.episodes(), 1, "the conversation project has an episode");
    f.note("Luna's vet", "Luna sees the vet on Elm Street.", &[]);
    let integration = f
        .store
        .read(|db| graphicle_runtime::code_graph_arm::integration_default(db, &f.repo))
        .unwrap();
    assert_eq!(integration["integrated"], false);
    assert_eq!(integration["reason"], "no_code_symbols");
    let m = provider().await;
    let question = "When did Luna see the vet?";
    let default = f.basic(&m, json!({"question":question})).await;
    let before = f
        .basic(
            &m,
            json!({"question":question,"memory_retrieval":false,"graph_retrieval":false}),
        )
        .await;
    assert_eq!(default["integration"]["default"], "source_only");
    assert_eq!(default["integration"]["reason"], "no_code_symbols");
    assert_eq!(default["integration"]["requested"], json!([]));
    for field in [
        "evidence",
        "sources",
        "memories",
        "code_graph",
        "coverage",
        "arms",
        "evidence_tokens",
        "graph_source",
    ] {
        assert_eq!(default[field], before[field], "{field}");
    }
    assert_eq!(default["mechanisms"], before["mechanisms"]);
    assert_eq!(default["mechanisms"]["reviewed_memory_enabled"], false);
    assert_eq!(default["mechanisms"]["code_graph_searched"], false);
    assert_eq!(default["code_graph"]["status"], "disabled");
}

/// The declared change for `context` on projects without code: Markdown sections are document nodes of the episode, so
/// once a sync projects a notes folder the code arm reads them, as it already
/// did wherever a worker had projected the folder. `.txt` sessions give
/// no node, so stores that hold `.txt` sessions only are unchanged.
#[test]
fn context_on_markdown_reads_its_sections_once_projected_and_txt_gives_no_node() {
    let session = "Conversation date: 2023/05/20\n\nuser: I adopted a cat named Luna.\n";
    let question = "Where does Luna see the vet?";
    let notes = Fixture::new(&[
        ("session-1.txt", session),
        (
            "notes.md",
            "# Notes\n\n## Luna's vet\n\nLuna sees the vet on Elm Street.\n",
        ),
    ]);
    assert_eq!(
        notes.context(question)["code_graph"]["status"],
        "no_episode"
    );
    notes.drain();
    let projected = notes.context(question);
    assert_eq!(
        projected["code_graph"]["status"], "fused",
        "{}",
        projected["code_graph"]
    );
    let facts = cited(&projected);
    assert!(!facts.is_empty(), "{}", projected["evidence"]);
    assert!(
        facts.iter().all(|fact| fact["relpath"] == "notes.md"),
        "{facts:?}"
    );

    let txt = Fixture::new(&[("session-1.txt", session)]);
    txt.drain();
    assert_eq!(txt.episodes(), 1);
    let packet = txt.context(question);
    assert_eq!(
        packet["code_graph"]["status"], "no_match",
        "{}",
        packet["code_graph"]
    );
    assert_eq!(packet["graph_source"], "episode");
}

/// A `basic` answer whose code graph cannot be read is answered
/// from its sources, as a conversation project's, and says why, instead of
/// failing the job.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn basic_answers_from_sources_when_the_code_graph_cannot_be_read() {
    let f = Fixture::new(&[("a.py", A_V1), ("b.py", B)]);
    f.drain();
    let episode = f.current();
    // The recorded episode cannot be reopened, and no copy is resident.
    f.store
        .connect()
        .unwrap()
        .execute(
            "UPDATE gf_episodes SET attributes='not json' WHERE repository_id=?",
            [&f.repo],
        )
        .unwrap();
    graphicle::session::registry().close(&graph_projection::graph_ref(&f.repo, &episode));
    let m = provider().await;
    let packet = f
        .basic(&m, json!({"question":"where is beta defined"}))
        .await;
    let integration = &packet["integration"];
    assert_eq!(integration["default"], "source_only", "{integration}");
    assert_eq!(integration["reason"], "error", "{integration}");
    assert!(integration["error"].is_string(), "{integration}");
    assert_eq!(packet["code_graph"]["status"], "disabled");
    assert_eq!(packet["mechanisms"]["reviewed_memory_enabled"], false);
}

/// Fourth case: MCP `recall_project` honours
/// `graph_retrieval`, `memory_retrieval` and `include`. `include` is
/// published; the two switches are accepted beside the contract, which
/// leaves them out.
#[test]
fn mcp_recall_honours_the_switches_and_include() {
    let f = Fixture::new(&[
        ("a.py", A_V1),
        ("b.py", B),
        ("docs/guide.md", "# Guide\n\nbeta is defined in a.py.\n"),
    ]);
    f.drain();
    f.note(
        "beta is stable",
        "beta() in a.py always returns 2.",
        &["a.py"],
    );
    let app = App::new(f.store.clone(), model::Config::default(), vec![]).unwrap();
    let request = |method: &str, params: Value| {
        api::mcp(
            &app,
            &f.workspace,
            &f.key,
            &f.repo,
            &json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}),
        )
        .unwrap()
    };
    let recall = |arguments: Value| {
        let reply = request(
            "tools/call",
            json!({"name":"recall_project","arguments":arguments}),
        );
        assert_eq!(reply["result"]["isError"], false, "{reply}");
        reply["result"]["structuredContent"].clone()
    };
    let listed = request("tools/list", json!({}));
    let schema = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "recall_project")
        .unwrap()["inputSchema"]["properties"]
        .clone();
    assert!(schema.get("include").is_some(), "{schema}");
    for hidden in api::RECALL_SWITCHES {
        assert!(schema.get(hidden).is_none(), "{hidden} is published");
    }

    let question = "where is beta defined";
    let on = recall(json!({"question":question}));
    assert_eq!(on["code_graph"]["status"], "fused");
    assert_eq!(on["mechanisms"]["reviewed_memory_enabled"], true);
    assert_eq!(on["mechanisms"]["reviewed_memory_available"], 1);
    let graph_off = recall(json!({"question":question,"graph_retrieval":false}));
    assert_eq!(graph_off["code_graph"]["status"], "disabled");
    assert_eq!(graph_off["mechanisms"]["graph_retrieval_enabled"], false);
    assert!(cited(&graph_off).is_empty());
    let memory_off = recall(json!({"question":question,"memory_retrieval":false}));
    assert_eq!(memory_off["mechanisms"]["reviewed_memory_enabled"], false);
    assert_eq!(memory_off["mechanisms"]["reviewed_memory_available"], 0);
    assert_eq!(memory_off["code_graph"]["status"], "fused");
    let docs = recall(json!({"question":question,"include":["file:docs/"]}));
    assert_eq!(
        docs["filter"]["documents_excluded"], 2,
        "{}",
        docs["filter"]
    );
    assert!(
        docs["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .all(|window| window["id"] == "file:docs/guide.md"),
        "{}",
        docs["evidence"]
    );
    // A switch is a boolean, and the published contract still binds the rest.
    for bad in [
        json!({"question":question,"graph_retrieval":"no"}),
        json!({"question":question,"include":[]}),
        json!({"question":question,"unknown":true}),
    ] {
        let reply = request(
            "tools/call",
            json!({"name":"recall_project","arguments":bad}),
        );
        assert!(reply["error"]["message"].is_string(), "{bad}: {reply}");
    }
}

fn graphicle(home: &Path, data: &Path, cwd: &Path, args: &[&str], input: &str) -> (Value, String) {
    use std::io::Write;
    let mut command = Command::new(env!("CARGO_BIN_EXE_graphicle"));
    command
        .current_dir(cwd)
        .env("HOME", home)
        .arg("--data-dir")
        .arg(data)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = crate::fork_guard::spawn(&mut command).unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(output.status.success(), "{args:?}: {stderr}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value = serde_json::from_str(&stdout).unwrap_or_else(|_| json!(stdout.to_string()));
    (value, stderr)
}

/// A folder whose `settle_invoice` calls a helper in another file that
/// shares no word with the question below, and notes that do.
fn billing(root: &Path) {
    std::fs::create_dir_all(root).unwrap();
    std::fs::write(
        root.join("invoice.py"),
        "from money import quantize_cents\n\n\ndef settle_invoice(total, tax):\n    \"\"\"Settle an invoice: the amount due with tax.\"\"\"\n    return quantize_cents(total + tax)\n",
    )
    .unwrap();
    std::fs::write(
        root.join("money.py"),
        "def quantize_cents(value):\n    scaled = int(value * 100 + 0.5)\n    return scaled / 100\n",
    )
    .unwrap();
    std::fs::write(
        root.join("README.md"),
        "# Billing\n\nInvoices are settled once a month. Each invoice lists its tax.\nsettle_invoice is the entry point of the monthly run.\n",
    )
    .unwrap();
    for i in 1..=10 {
        std::fs::write(
            root.join(format!("note{i}.md")),
            format!(
                "# Note {i}\n\nThe settle invoice run depends on the monthly helper schedule {i}.\n"
            ),
        )
        .unwrap();
    }
}

/// The end-to-end check: sync a folder and
/// connect Claude Code in it, with no `start`, `serve` or `worker`. A stdio
/// session's `recall_project` reports the arm `fused`, never `no_episode`,
/// and on a question that names a function only by what its caller does it
/// cites that function, which retrieval without the graph leaves out.
#[test]
fn a_synced_folder_serves_its_code_graph_over_stdio_without_a_worker() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().canonicalize().unwrap();
    let data = home.join("data");
    let folder = home.join("billing");
    billing(&folder);
    let (synced, progress) = graphicle(&home, &data, &folder, &["sync", "."], "");
    assert_eq!(synced["graph"]["status"], "built", "{synced:#}");
    assert!(synced["graph"]["nodes"].as_u64().unwrap_or(0) > 0);
    assert!(
        progress.contains("graphicle: building the code graph of 13 files")
            && progress.contains("graphicle: code graph ready:"),
        "{progress}"
    );
    let (connected, _) = graphicle(&home, &data, &folder, &["connect", "claude-code"], "");
    assert_eq!(connected["entry"], "graphicle", "{connected:#}");
    let entry: Value =
        serde_json::from_slice(&std::fs::read(folder.join(".mcp.json")).unwrap()).unwrap();
    let args: Vec<String> = entry["mcpServers"]["graphicle"]["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|arg| arg.as_str().unwrap().to_string())
        .collect();
    let question = "Which helper does settle_invoice depend on?";
    let requests = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"i3","version":"0"}}}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"recall_project","arguments":{"question":question}}}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"recall_project","arguments":{"question":question,"graph_retrieval":false}}}),
    ]
    .map(|request| request.to_string())
    .join("\n")
        + "\n";
    // The entry's own command line, with this program; `--data-dir` is in
    // its arguments because the store is not in the default folder.
    let mut command = Command::new(env!("CARGO_BIN_EXE_graphicle"));
    command
        .current_dir(&folder)
        .env("HOME", &home)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = crate::fork_guard::spawn(&mut command).unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(requests.as_bytes())
            .unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let replies: Vec<Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let packet = |id: u64| {
        replies.iter().find(|reply| reply["id"] == id).unwrap()["result"]["structuredContent"]
            .clone()
    };
    let with_graph = packet(2);
    assert_eq!(
        with_graph["code_graph"]["status"], "fused",
        "{}",
        with_graph["code_graph"]
    );
    assert_eq!(with_graph["code_graph"]["episode_lag"], 0);
    let helper = cited(&with_graph)
        .into_iter()
        .find(|fact| fact["label"] == "quantize_cents()")
        .unwrap_or_else(|| panic!("the helper is not cited: {}", with_graph["evidence"]));
    assert_eq!(helper["relpath"], "money.py");
    assert_eq!(helper["hop"], 1, "{helper}");
    let without = packet(3);
    assert_eq!(without["code_graph"]["status"], "disabled");
    assert!(
        without["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .all(|window| window["id"] != "file:money.py"),
        "retrieval without the graph found the helper: {}",
        without["evidence"]
    );
}

/// `sync` builds in the foreground by default; `--no-graph` leaves the
/// projection queued for later, and `--background` returns at once while a
/// detached `graphicle build-graph` records the episode. `ask` builds a
/// queued graph before it answers (`ask.rs`).
#[test]
fn sync_builds_the_graph_now_later_or_in_the_background() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().canonicalize().unwrap();
    let data = home.join("data");
    let folder = home.join("app");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("a.py"), A_V1).unwrap();
    std::fs::write(folder.join("b.py"), B).unwrap();
    let (later, _) = graphicle(&home, &data, &folder, &["sync", ".", "--no-graph"], "");
    assert_eq!(later["graph"]["status"], "queued", "{later:#}");
    let project = s(&later, "repository_id").to_string();
    // The same tree again, in the foreground: the waiting projection runs.
    let (now, _) = graphicle(&home, &data, &folder, &["sync", "."], "");
    assert_eq!(now["graph"]["status"], "built", "{now:#}");
    assert_eq!(now["graph"]["episode"], later["graph"]["episode"]);
    let (again, progress) = graphicle(&home, &data, &folder, &["sync", "."], "");
    assert_eq!(again["graph"]["status"], "current", "{again:#}");
    assert!(progress.is_empty(), "{progress}");
    std::fs::write(folder.join("a.py"), A_V2).unwrap();
    let (background, _) = graphicle(&home, &data, &folder, &["sync", ".", "--background"], "");
    assert_eq!(
        background["graph"]["status"], "background",
        "{background:#}"
    );
    let episode = s(&background["graph"], "episode").to_string();
    assert_ne!(episode, s(&now["graph"], "episode"));
    let store = Store::open(&data).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while !store
        .read(|db| graph_projection::is_recorded(db, &project, &episode))
        .unwrap()
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the background build did not record {episode}"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let mut refused = Command::new(env!("CARGO_BIN_EXE_graphicle"));
    refused
        .current_dir(&folder)
        .env("HOME", &home)
        .arg("--data-dir")
        .arg(&data)
        .args(["sync", ".", "--background", "--no-graph"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let refused = crate::fork_guard::spawn(&mut refused)
        .unwrap()
        .wait_with_output()
        .unwrap();
    assert!(!refused.status.success());
}

/// A `graphicle mcp` session in `folder`, with its standard input kept open.
struct Session {
    child: std::process::Child,
    replies: std::io::BufReader<std::process::ChildStdout>,
}
impl Session {
    fn start(home: &Path, data: &Path, folder: &Path) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_graphicle"));
        command
            .current_dir(folder)
            .env("HOME", home)
            .arg("--data-dir")
            .arg(data)
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = crate::fork_guard::spawn(&mut command).unwrap();
        let replies = std::io::BufReader::new(child.stdout.take().unwrap());
        Self { child, replies }
    }
    fn call(&mut self, request: &Value) -> Value {
        use std::io::{BufRead, Write};
        let input = self.child.stdin.as_mut().unwrap();
        writeln!(input, "{request}").unwrap();
        input.flush().unwrap();
        let mut line = String::new();
        self.replies.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or_else(|_| panic!("not a reply: {line:?}"))
    }
    /// Wait for the process, at most two minutes.
    fn wait(mut self) -> (std::process::ExitStatus, String) {
        use std::io::Read;
        let deadline = Instant::now() + Duration::from_secs(120);
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "graphicle mcp did not exit");
            std::thread::sleep(Duration::from_millis(50));
        };
        let mut stderr = String::new();
        self.child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        (status, stderr)
    }
}

fn running_jobs(store: &Store) -> i64 {
    store
        .connect()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM am_jobs WHERE state='running'",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

/// After `sync --no-graph`, a `graphicle mcp`
/// session builds the waiting graph at start, beside its requests, with no
/// worker; its recall then reads the new episode with no lag. Closing the
/// input ends the session without leaving a job running.
#[test]
fn mcp_builds_a_waiting_graph_at_start_and_leaves_no_job_running() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().canonicalize().unwrap();
    let data = home.join("data");
    let folder = home.join("app");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("a.py"), A_V1).unwrap();
    std::fs::write(folder.join("b.py"), B).unwrap();
    let (later, _) = graphicle(&home, &data, &folder, &["sync", ".", "--no-graph"], "");
    assert_eq!(later["graph"]["status"], "queued", "{later:#}");
    let project = s(&later, "repository_id").to_string();
    let episode = s(&later["graph"], "episode").to_string();

    let mut session = Session::start(&home, &data, &folder);
    let store = Store::open(&data).unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    while !store
        .read(|db| graph_projection::is_recorded(db, &project, &episode))
        .unwrap()
    {
        assert!(Instant::now() < deadline, "mcp did not build {episode}");
        std::thread::sleep(Duration::from_millis(50));
    }
    let reply = session.call(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
        "params":{"name":"recall_project","arguments":{"question":"who calls beta"}}}));
    let arm = &reply["result"]["structuredContent"]["code_graph"];
    assert_eq!(arm["status"], "fused", "{reply}");
    assert_eq!(arm["episode"], episode.as_str(), "{arm}");
    assert_eq!(arm["episode_lag"], 0, "{arm}");

    drop(session.child.stdin.take());
    let (status, stderr) = session.wait();
    assert!(status.success(), "{stderr}");
    assert_eq!(running_jobs(&store), 0);
}

/// SIGTERM ends a `graphicle mcp` session as a closed input does.
/// The job in hand is finished, so its lease is released instead of left
/// to expire (240 s) while the next sync waits on it.
#[cfg(unix)]
#[test]
fn mcp_on_sigterm_finishes_the_job_in_hand_and_leaves_none_running() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().canonicalize().unwrap();
    let data = home.join("data");
    let folder = home.join("app");
    std::fs::create_dir_all(&folder).unwrap();
    // Enough files that the projection is still running when the signal
    // arrives.
    for i in 0..400 {
        std::fs::write(
            folder.join(format!("m{i}.py")),
            format!(
                "from m{next} import f{next}\n\n\ndef f{i}(value):\n    return f{next}(value) + {i}\n\n\nclass C{i}:\n    def run(self):\n        return f{i}(1)\n",
                next = (i + 1) % 400
            ),
        )
        .unwrap();
    }
    let (later, _) = graphicle(&home, &data, &folder, &["sync", ".", "--no-graph"], "");
    assert_eq!(later["graph"]["status"], "queued", "{later:#}");
    let project = s(&later, "repository_id").to_string();
    let episode = s(&later["graph"], "episode").to_string();
    let store = Store::open(&data).unwrap();
    let state = |store: &Store| -> String {
        store
            .connect()
            .unwrap()
            .query_row(
                "SELECT state FROM am_jobs WHERE repository_id=? AND kind='project' ORDER BY created_at DESC LIMIT 1",
                [&project],
                |r| r.get(0),
            )
            .unwrap()
    };

    let session = Session::start(&home, &data, &folder);
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut seen = state(&store);
    while seen == "queued" {
        assert!(
            Instant::now() < deadline,
            "mcp did not start the projection"
        );
        std::thread::sleep(Duration::from_millis(5));
        seen = state(&store);
    }
    let signalled_while_running = seen == "running";
    let killed = crate::fork_guard::spawn(
        Command::new("kill")
            .args(["-TERM", &session.child.id().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    )
    .unwrap()
    .wait()
    .unwrap();
    assert!(killed.success());
    let (status, stderr) = session.wait();
    assert!(status.success(), "{status:?}: {stderr}");
    assert_eq!(running_jobs(&store), 0, "{stderr}");
    assert_eq!(state(&store), "complete");
    assert!(
        store
            .read(|db| graph_projection::is_recorded(db, &project, &episode))
            .unwrap()
    );
    // Not an assertion on timing: a fast machine may finish first.
    eprintln!("signalled while the projection ran: {signalled_while_running}");
}
