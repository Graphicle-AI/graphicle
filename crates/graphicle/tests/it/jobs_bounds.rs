//! G08: bounded asynchronous ingestion on the `am_jobs` queue.

use graphicle_runtime::{
    Error, ingestion, model,
    store::{Store, s},
};
use serde_json::{Value, json};
use tempfile::TempDir;

const A: &str = "def alpha():\n    return 1\n";

struct Fixture {
    _dir: TempDir,
    store: Store,
    workspace: String,
    key: String,
    repo: String,
    config: model::Config,
}
impl Fixture {
    fn new(config: model::Config) -> Self {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let ws = store.create_workspace("Job bounds").unwrap();
        let workspace = s(&ws, "id").to_string();
        let key = s(&ws, "capability").to_string();
        let f = Self {
            _dir: dir,
            store,
            workspace,
            key,
            repo: String::new(),
            config,
        };
        let repo = f.register("bounds");
        Self { repo, ..f }
    }
    fn register(&self, project_key: &str) -> String {
        let repo = s(
            &self
                .store
                .register(
                    &self.workspace,
                    &self.key,
                    &json!({"project_key":project_key,"name":project_key}),
                )
                .unwrap(),
            "id",
        )
        .to_string();
        self.sync(&repo, &[("a.py", A)]);
        repo
    }
    fn sync(&self, repo: &str, files: &[(&str, &str)]) {
        let state = self
            .store
            .project_status(&self.workspace, &self.key, &json!({"repository_id":repo}))
            .unwrap();
        let begin=self.store.sync_prepare(&self.workspace,&self.key,&json!({"repository_id":repo,"expected_snapshot":state["repository"]["snapshot"],"manifest":files.iter().map(|(name,text)|json!({"path":name,"sha256":graphicle_memory::text::sha256(text.as_bytes()),"bytes":text.len()})).collect::<Vec<_>>()})).unwrap();
        self.store.sync_upload(&self.workspace,&self.key,&json!({"repository_id":repo,"sync_id":begin["sync_id"],"files":files.iter().map(|(name,text)|json!({"path":name,"content":text})).collect::<Vec<_>>()})).unwrap();
        self.store
            .sync_commit(
                &self.workspace,
                &self.key,
                &json!({"repository_id":repo,"sync_id":begin["sync_id"]}),
            )
            .unwrap();
    }
    fn enqueue(&self, repo: &str, mut args: Value) -> graphicle_runtime::Result<Value> {
        args["repository_id"] = json!(repo);
        if args.get("kind").is_none() {
            args["kind"] = json!("basic");
        }
        if args.get("semantic_retrieval").is_none() {
            args["semantic_retrieval"] = json!(false);
        }
        self.store
            .enqueue(&self.workspace, &self.key, &args, &self.config)
    }
    fn db(&self) -> rusqlite::Connection {
        self.store.connect().unwrap()
    }
    fn row(&self, id: &str) -> Value {
        let (state, attempt, not_before): (String, i64, String) = self
            .db()
            .query_row(
                "SELECT state,attempt,not_before FROM am_jobs WHERE id=?",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        json!({"state":state,"attempt":attempt,"not_before":not_before})
    }
    fn count(&self, repo: &str, kind: &str) -> i64 {
        self.db()
            .query_row(
                "SELECT count(*) FROM am_jobs WHERE repository_id=? AND kind=?",
                [repo, kind],
                |r| r.get(0),
            )
            .unwrap()
    }
    fn make_ready(&self, id: &str) {
        self.db()
            .execute(
                "UPDATE am_jobs SET not_before='1970-01-01T00:00:00.000000Z' WHERE id=?",
                [id],
            )
            .unwrap();
    }
}

fn config(bounds: Option<usize>) -> model::Config {
    let profile: model::Profile = serde_json::from_value(json!({"protocol":"chat","base_url":"http://127.0.0.1:1","model":"fixture","input_limit":4000,"output_limit":1000,"temperature":0})).unwrap();
    model::Config {
        default_model: Some("fixture".into()),
        models: [("fixture".into(), profile)].into(),
        max_queued_bytes_per_repository: bounds,
        ..Default::default()
    }
}

fn seconds_from_now(stamp: &str) -> i64 {
    let at = chrono::DateTime::parse_from_rfc3339(stamp).unwrap();
    (at.with_timezone(&chrono::Utc) - chrono::Utc::now()).num_seconds()
}

#[test]
fn identical_jobs_replay_instead_of_duplicating() {
    let f = Fixture::new(config(None));
    let args = json!({"question":"what does alpha do","max_calls":1});
    let first = f.enqueue(&f.repo, args.clone()).unwrap();
    assert_eq!(first["replayed"], false);
    let second = f.enqueue(&f.repo, args.clone()).unwrap();
    assert_eq!(second["replayed"], true, "{second}");
    assert_eq!(second["id"], first["id"]);
    assert_eq!(f.count(&f.repo, "basic"), 1);

    // A different payload for the same kind is genuinely new work.
    let third = f
        .enqueue(
            &f.repo,
            json!({"question":"what does beta do","max_calls":1}),
        )
        .unwrap();
    assert_eq!(third["replayed"], false);
    assert_ne!(third["id"], first["id"]);
    assert_eq!(f.count(&f.repo, "basic"), 2);

    // Interactive operations are never queued.
    for kind in ingestion::INTERACTIVE {
        let error = f
            .enqueue(&f.repo, json!({"kind":kind,"question":"q"}))
            .unwrap_err();
        assert_eq!(error.status, 400, "{kind}");
        assert!(
            error.message.contains("never queued"),
            "{kind}: {}",
            error.message
        );
    }
}

#[test]
fn one_job_per_repository_runs_at_a_time() {
    let f = Fixture::new(config(None));
    let other = f.register("other");
    let a1 = f
        .enqueue(&f.repo, json!({"question":"first","max_calls":1}))
        .unwrap();
    let a2 = f
        .enqueue(&f.repo, json!({"question":"second","max_calls":1}))
        .unwrap();
    let b1 = f
        .enqueue(&other, json!({"question":"elsewhere","max_calls":1}))
        .unwrap();

    let running = f.store.lease_job().unwrap().unwrap();
    assert_eq!(running.id, a1["id"], "oldest ready job leases first");
    // The same repository offers nothing while one of its jobs is running.
    let next = f.store.lease_job().unwrap().unwrap();
    assert_eq!(next.repository, other);
    assert_eq!(next.id, b1["id"]);
    assert!(
        f.store.lease_job().unwrap().is_none(),
        "both repositories have running work"
    );
    f.store.finish_job(&next, Ok(json!({}))).unwrap();
    assert!(f.store.lease_job().unwrap().is_some_and(|job| {
        // The other repository's projection (priority -1) is now free to run.
        job.repository == other && job.kind == "project"
    }));

    f.store
        .finish_job(&running, Ok(json!({"done":true})))
        .unwrap();
    let after = f.store.lease_job().unwrap().unwrap();
    assert_eq!(after.repository, f.repo);
    assert_eq!(after.id, a2["id"], "second job in the repository follows");
    assert_eq!(f.row(&running.id.clone())["state"], "complete");

    // Admission exclusion keeps a worker off kinds it has no permit for.
    f.store.finish_job(&after, Ok(json!({}))).unwrap();
    assert!(f.store.lease_job_excluding(&["project"]).unwrap().is_none());
    let projection = f.store.lease_job().unwrap().unwrap();
    assert_eq!(projection.kind, "project");
}

#[test]
fn independent_observers_wait_for_opt_in_and_disjoint_prefixes() {
    let cases = [
        (json!(["file:a"]), json!(["file:b"]), None, None, false),
        (
            json!(["file:a"]),
            json!(["file:b"]),
            Some(true),
            None,
            false,
        ),
        (
            json!(["file:a"]),
            json!(["file:b"]),
            Some(false),
            Some(true),
            false,
        ),
        (
            json!(["file:a"]),
            json!(["file:b"]),
            Some(true),
            Some(true),
            true,
        ),
        (
            json!(["file:session-"]),
            json!(["file:session-01.txt"]),
            Some(true),
            Some(true),
            false,
        ),
        (
            json!(["file:session-01.txt"]),
            json!(["file:session-"]),
            Some(true),
            Some(true),
            false,
        ),
        (
            json!(["file:a", "file:z"]),
            json!(["file:z.txt"]),
            Some(true),
            Some(true),
            false,
        ),
        (
            json!(["file:a"]),
            json!(["file:a"]),
            Some(true),
            Some(true),
            false,
        ),
        (
            json!(["file:a_%"]),
            json!(["file:ab"]),
            Some(true),
            Some(true),
            true,
        ),
        (
            json!(["file:é"]),
            json!(["file:épisode"]),
            Some(true),
            Some(true),
            false,
        ),
        (
            json!(["file:a\u{0}"]),
            json!(["file:a\u{0}b"]),
            Some(true),
            Some(true),
            false,
        ),
    ];
    for (a, b, a_flag, b_flag, independent) in cases {
        let f = Fixture::new(config(None));
        let mut ids = Vec::new();
        for (index, (prefixes, flag)) in [(a, a_flag), (b, b_flag)].into_iter().enumerate() {
            let mut args =
                json!({"kind":"observer_notes","include":prefixes,"max_calls":1,"round":index});
            if let Some(flag) = flag {
                args["parallel_sources"] = json!(flag);
            }
            ids.push(f.enqueue(&f.repo, args).unwrap()["id"].clone());
        }
        let first = f.store.lease_job().unwrap().unwrap();
        assert_eq!(first.id, ids[0]);
        assert_eq!(
            f.store.peek_ready_kind().unwrap().as_deref(),
            independent.then_some("observer_notes"),
            "admission and leasing must apply the same source conflict rule"
        );
        let second = f.store.lease_job().unwrap();
        assert_eq!(second.is_some(), independent);
        if let Some(second) = second {
            assert_eq!(second.id, ids[1]);
        }
    }
}

#[test]
fn independent_observers_block_other_work_until_every_live_lease_finishes() {
    let f = Fixture::new(config(None));
    for source in ["file:a", "file:b"] {
        f.enqueue(&f.repo, json!({"kind":"observer_notes","include":[source],"parallel_sources":true,"max_calls":1})).unwrap();
    }
    let answer = f
        .enqueue(
            &f.repo,
            json!({"question":"answer after both observations"}),
        )
        .unwrap();
    let first = f.store.lease_job().unwrap().unwrap();
    let second = f.store.lease_job().unwrap().unwrap();
    assert_eq!(first.kind, "observer_notes");
    assert_eq!(second.kind, "observer_notes");
    assert!(f.store.peek_ready_kind().unwrap().is_none());
    assert!(
        f.store
            .lease_job_of_kind(&f.repo, "basic")
            .unwrap()
            .is_none()
    );
    f.store.finish_job(&first, Ok(json!({}))).unwrap();
    assert!(f.store.peek_ready_kind().unwrap().is_none());
    assert!(
        f.store
            .lease_job_excluding(&["observer_notes"])
            .unwrap()
            .is_none()
    );
    f.store.finish_job(&second, Ok(json!({}))).unwrap();
    assert_eq!(f.store.peek_ready_kind().unwrap().as_deref(), Some("basic"));
    assert_eq!(f.store.lease_job().unwrap().unwrap().id, answer["id"]);
}

#[test]
fn observer_parallelism_rejects_ambiguous_values_and_other_workflows() {
    let f = Fixture::new(config(None));
    for flag in [json!(null), json!(1), json!("true"), json!([]), json!({})] {
        let error = f
            .enqueue(
                &f.repo,
                json!({"kind":"observer_notes","include":["file:a"],"parallel_sources":flag}),
            )
            .unwrap_err();
        assert_eq!(error.status, 400);
        assert!(error.message.contains("parallel_sources must be a boolean"));
    }
    for flag in [false, true] {
        let error = f
            .enqueue(&f.repo, json!({"question":"q","parallel_sources":flag}))
            .unwrap_err();
        assert_eq!(error.status, 400);
        assert!(error.message.contains("only to observer_notes"));
    }
    assert_eq!(f.count(&f.repo, "observer_notes"), 0);
    assert_eq!(f.count(&f.repo, "basic"), 0);
}

#[test]
fn lease_filters_apply_before_the_candidate_limit() {
    let f = Fixture::new(config(None));
    let other = f.register("busy");
    let third = f.register("also-busy");
    for i in 0..101 {
        f.enqueue(
            if i < 99 { &other } else { &third },
            json!({"question":format!("Queued question {i}"),"max_calls":1}),
        )
        .unwrap();
    }
    let selected = f
        .store
        .lease_job_of_kind(&f.repo, "project")
        .unwrap()
        .expect("unrelated queued questions must not hide this repository's projection");
    assert_eq!(selected.repository, f.repo);
    assert_eq!(selected.kind, "project");
    f.store.finish_job(&selected, Ok(json!({}))).unwrap();
    let admitted = f
        .store
        .lease_job_excluding(&["basic"])
        .unwrap()
        .expect("excluded work must not consume the candidate limit");
    assert_eq!(admitted.repository, other);
    assert_eq!(admitted.kind, "project");
    assert_eq!(f.count(&other, "basic"), 99);
}

#[test]
fn idle_workers_notice_expired_leases_without_another_queued_job() {
    let f = Fixture::new(config(None));
    let expired = f.store.lease_job().unwrap().unwrap();
    f.db()
        .execute(
            "UPDATE am_jobs SET lease_until='1970-01-01T00:00:00.000000Z' WHERE id=?",
            [&expired.id],
        )
        .unwrap();
    assert_eq!(
        f.store.peek_ready_kind().unwrap().as_deref(),
        Some("project")
    );
    let retry = f.store.lease_job().unwrap().unwrap();
    assert_eq!(retry.id, expired.id);
    assert_ne!(retry.lease, expired.lease);
    f.db().execute("UPDATE am_jobs SET attempt=max_attempts,lease_until='1970-01-01T00:00:00.000000Z' WHERE id=?", [&retry.id]).unwrap();
    assert!(f.store.peek_ready_kind().unwrap().is_none());
    assert_eq!(f.row(&retry.id)["state"], "failed");
}

#[test]
fn failures_retry_with_exponential_backoff() {
    let f = Fixture::new(config(None));
    let job = f
        .enqueue(
            &f.repo,
            json!({"question":"flaky","max_calls":1,"max_attempts":3}),
        )
        .unwrap();
    let id = s(&job, "id").to_string();

    let leased = f.store.lease_job().unwrap().unwrap();
    assert_eq!(leased.id, id);
    f.store
        .finish_job(&leased, Err(Error::bad("boom")))
        .unwrap();
    let row = f.row(&id);
    assert_eq!(row["state"], "retry", "{row}");
    assert_eq!(row["attempt"], 1);
    let delay = seconds_from_now(s(&row, "not_before"));
    assert!(
        (27..=30).contains(&delay),
        "first backoff ≈30 s, got {delay}"
    );
    // A backed-off job is not leased before `not_before`; only the
    // repository's pending projection is offered, and it is drained here.
    let projection = f.store.lease_job().unwrap().unwrap();
    assert_eq!(projection.kind, "project");
    f.store.finish_job(&projection, Ok(json!({}))).unwrap();
    assert!(f.store.lease_job().unwrap().is_none());

    f.make_ready(&id);
    let leased = f.store.lease_job().unwrap().unwrap();
    assert_eq!(leased.id, id);
    f.store
        .finish_job(&leased, Err(Error::bad("boom again")))
        .unwrap();
    let row = f.row(&id);
    assert_eq!(row["state"], "retry", "{row}");
    assert_eq!(row["attempt"], 2);
    let delay = seconds_from_now(s(&row, "not_before"));
    assert!(
        (57..=60).contains(&delay),
        "second backoff ≈60 s, got {delay}"
    );

    f.make_ready(&id);
    let leased = f.store.lease_job().unwrap().unwrap();
    f.store
        .finish_job(&leased, Err(Error::bad("gave up")))
        .unwrap();
    let row = f.row(&id);
    assert_eq!(row["state"], "failed", "max_attempts=3 exhausted: {row}");
    assert_eq!(row["attempt"], 3);

    // The schedule itself: doubling from 30 s, capped.
    assert_eq!(ingestion::backoff(1).num_seconds(), 30);
    assert_eq!(ingestion::backoff(2).num_seconds(), 60);
    assert_eq!(ingestion::backoff(3).num_seconds(), 120);
    assert_eq!(
        ingestion::backoff(40).num_seconds(),
        ingestion::MAX_BACKOFF_SECONDS
    );
}

#[test]
fn queued_payload_bytes_are_bounded_per_repository() {
    let f = Fixture::new(config(Some(ingestion::MIN_QUEUED_BYTES)));
    let other = f.register("other");
    let padding = |seed: char| json!({"question":"large","padding":seed.to_string().repeat(40_000),"max_calls":1});

    let first = f.enqueue(&f.repo, padding('a')).unwrap();
    assert_eq!(first["replayed"], false);
    let queued = ingestion::queued_bytes(&f.db(), &f.repo).unwrap();
    assert!(
        queued > 40_000 && queued < ingestion::MIN_QUEUED_BYTES,
        "{queued}"
    );

    let error = f.enqueue(&f.repo, padding('b')).unwrap_err();
    assert_eq!(error.status, 400, "{}", error.message);
    assert!(
        error.message.contains("job queue is full"),
        "{}",
        error.message
    );
    assert!(
        error
            .message
            .contains(&ingestion::MIN_QUEUED_BYTES.to_string())
    );
    assert_eq!(f.count(&f.repo, "basic"), 1, "rejected work is not stored");

    // Replays never hit the bound; other repositories have their own budget.
    assert_eq!(f.enqueue(&f.repo, padding('a')).unwrap()["replayed"], true);
    assert_eq!(f.enqueue(&other, padding('b')).unwrap()["replayed"], false);

    // Finishing queued work frees the budget.
    let leased = f.store.lease_job().unwrap().unwrap();
    assert_eq!(leased.id, first["id"]);
    f.store.finish_job(&leased, Ok(json!({}))).unwrap();
    assert!(f.enqueue(&f.repo, padding('c')).is_ok());

    // Bounds below the minimum are rejected at configuration time.
    let mut bad = config(Some(1024));
    assert!(bad.validate().is_err());
    bad.max_queued_bytes_per_repository = None;
    bad.max_cpu_jobs = Some(0);
    assert!(bad.validate().is_err());
    assert!(config(None).validate().is_ok());
    let bounds = ingestion::Bounds::from_config(&config(None));
    assert_eq!(
        bounds.max_queued_bytes_per_repository,
        ingestion::DEFAULT_QUEUED_BYTES
    );
    assert!(bounds.max_cpu_jobs >= 1);
}
