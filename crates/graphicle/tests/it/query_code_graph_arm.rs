//! G04: the `code_graph` typed-graph arm fused into `context`.

use graphicle_runtime::store::{Store, s};
use serde_json::{Value, json};
use tempfile::TempDir;

const A_V1: &str = "def alpha():\n    return 1\n\ndef beta():\n    return 2\n";
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
        let ws = store.create_workspace("Code-graph arm").unwrap();
        let workspace = s(&ws, "id").to_string();
        let key = s(&ws, "capability").to_string();
        let repo = s(
            &store
                .register(
                    &workspace,
                    &key,
                    &json!({"project_key":"arm","name":"Arm sources"}),
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
    fn sync(&self, files: &[(&str, &str)]) {
        let state = self
            .store
            .project_status(&self.workspace, &self.key, &self.args(json!({})))
            .unwrap();
        let begin=self.store.sync_prepare(&self.workspace,&self.key,&self.args(json!({"expected_snapshot":state["repository"]["snapshot"],"manifest":files.iter().map(|(name,text)|json!({"path":name,"sha256":graphicle_memory::text::sha256(text.as_bytes()),"bytes":text.len()})).collect::<Vec<_>>()}))).unwrap();
        self.store.sync_upload(&self.workspace,&self.key,&self.args(json!({"sync_id":begin["sync_id"],"files":files.iter().map(|(name,text)|json!({"path":name,"content":text})).collect::<Vec<_>>()}))).unwrap();
        self.store
            .sync_commit(
                &self.workspace,
                &self.key,
                &self.args(json!({"sync_id":begin["sync_id"]})),
            )
            .unwrap();
    }
    fn project(&self) -> Value {
        self.store
            .graph_project(&self.workspace, &self.key, &self.repo)
            .unwrap()
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
}

fn code_graph_evidence(packet: &Value) -> Vec<&Value> {
    packet["evidence"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|window| window.get("code_graph"))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn projected_episode_yields_defining_node_evidence_with_identity() {
    let f = Fixture::new(&[("a.py", A_V1), ("b.py", B)]);
    f.project();
    let packet = f.context("where is beta defined");

    assert_eq!(
        packet["code_graph"]["status"], "fused",
        "{}",
        packet["code_graph"]
    );
    assert!(
        packet["code_graph"]["anchored"].as_u64().unwrap_or(0) > 0,
        "{}",
        packet["code_graph"]
    );
    let arms = &packet["arms"];
    assert!(
        arms["code_graph"]["candidates"].as_u64().unwrap_or(0) > 0,
        "arm contribution must be reported: {arms}"
    );

    let facts = code_graph_evidence(&packet);
    assert!(
        !facts.is_empty(),
        "no code-graph-annotated evidence: {packet}"
    );
    let beta = facts
        .iter()
        .find(|fact| s(fact, "qualified_name").ends_with("beta") || s(fact, "label") == "beta")
        .unwrap_or_else(|| panic!("beta not cited: {facts:?}"));
    assert_eq!(beta["relpath"], "a.py");
    assert!(
        beta["stable_id"].as_str().is_some_and(|id| !id.is_empty()),
        "{beta}"
    );
    assert!(
        beta["revision"].as_str().is_some_and(|rev| !rev.is_empty()),
        "{beta}"
    );
    assert!(beta["derivation"].is_string(), "{beta}");
    assert_eq!(beta["hop"], 0);
    // Every fused hit carries identity where the node has it.
    for fact in &facts {
        if fact["relpath"].is_string() {
            assert!(fact["stable_id"].is_string(), "{fact}");
            assert!(fact["revision"].is_string(), "{fact}");
        }
        assert!(fact["derivation"].is_string(), "{fact}");
    }
    assert!(
        packet["code_graph"]["cited"].as_u64().unwrap_or(0) >= 1,
        "{}",
        packet["code_graph"]
    );
}

#[test]
fn unrecorded_episode_skips_arm_with_diagnostic() {
    let f = Fixture::new(&[("a.py", A_V1), ("b.py", B)]);
    let packet = f.context("where is beta defined");
    assert_eq!(
        packet["code_graph"]["status"], "no_episode",
        "{}",
        packet["code_graph"]
    );
    assert!(packet["code_graph"]["episode"].is_string());
    assert!(
        packet["arms"].get("code_graph").is_none(),
        "{}",
        packet["arms"]
    );
    assert!(code_graph_evidence(&packet).is_empty());
    // The request still succeeds with the other arms.
    assert!(packet["evidence"].is_array() || packet["sources"].is_array());

    // Switching the graph arm off is reported distinctly, never as an error.
    let off = f
        .store
        .project_context(
            &f.workspace,
            &f.key,
            &f.args(json!({"question":"beta","graph_retrieval":false})),
        )
        .unwrap();
    assert_eq!(off["code_graph"]["status"], "disabled");
}

#[test]
fn foreign_principal_is_denied_without_graph_leak() {
    let f = Fixture::new(&[("a.py", A_V1), ("b.py", B)]);
    f.project();
    let other = f.store.create_workspace("Other").unwrap();
    let other_key = s(&other, "capability");

    // A stranger's capability against the first workspace.
    let error = f
        .store
        .project_context(&f.workspace, other_key, &f.args(json!({"question":"beta"})))
        .unwrap_err();
    assert_eq!(error.status, 403, "{}", error.message);
    assert!(!error.message.contains("beta"));
    assert!(!error.message.contains("stable_id"));

    // The stranger's own workspace cannot reach the repository either.
    let error = f
        .store
        .project_context(
            s(&other, "id"),
            other_key,
            &f.args(json!({"question":"beta"})),
        )
        .unwrap_err();
    // Existing `repo()` scoping reports an unreachable repository as a bad
    // request; nothing about the repository or its graph is disclosed.
    assert!(error.status.is_client_error(), "{}", error.message);
    assert_eq!(error.message, "Repository not found in this workspace");
    assert!(!error.message.contains("a.py"));

    // A bogus token never sees anything.
    let error = f
        .store
        .project_context(&f.workspace, "bogus", &f.args(json!({"question":"beta"})))
        .unwrap_err();
    assert_eq!(error.status, 403);
}

/// A skipped arm says why. The projection job of the
/// current head set is `queued`, `running`, `failed` or `none`, and the
/// packet names the latest recorded episode and its lag in files.
#[test]
fn a_skipped_arm_reports_projection_state_and_episode_lag() {
    let f = Fixture::new(&[("a.py", A_V1), ("b.py", B)]);
    let status = |f: &Fixture| f.context("where is beta defined")["code_graph"].clone();
    let set_job = |state: &str| {
        f.store
            .connect()
            .unwrap()
            .execute(
                "UPDATE am_jobs SET state=? WHERE kind='project' AND repository_id=?",
                rusqlite::params![state, f.repo],
            )
            .unwrap();
    };
    // Synced, never projected: the sync queued the projection. The job is
    // reported by state only: job listings show a job's id to its principal
    // and owners, never to every reader of the packet.
    let queued = status(&f);
    assert_eq!(queued["status"], "no_episode", "{queued}");
    assert_eq!(queued["projection"], "queued", "{queued}");
    assert!(queued.get("projection_job").is_none(), "{queued}");
    assert_eq!(queued["head_files"], 2);
    assert!(queued["latest_episode"].is_null());
    assert!(queued["episode_lag"].is_null());
    for (state, reported) in [
        ("running", "running"),
        ("failed", "failed"),
        ("complete", "none"),
    ] {
        set_job(state);
        let skipped = status(&f);
        assert_eq!(skipped["status"], "no_episode");
        assert_eq!(skipped["projection"], reported, "{state}: {skipped}");
    }
    // A recorded episode, then one file edited: with the lag fallback the
    // arm reads the latest episode while the new
    // projection waits, says it is one file behind, and cites only unchanged
    // files (`graph_always.rs`).
    let projected = f.project();
    let fused = status(&f);
    assert_eq!(fused["status"], "fused", "{fused}");
    assert_eq!(fused["episode_lag"], 0);
    f.sync(&[
        (
            "a.py",
            "def alpha():\n    return 10\n\ndef beta():\n    return 2\n",
        ),
        ("b.py", B),
    ]);
    let lagging = status(&f);
    assert_eq!(lagging["status"], "fused", "{lagging}");
    assert_eq!(lagging["projection"], "queued");
    assert_eq!(lagging["episode"], projected["episode"]);
    assert_eq!(lagging["episode_lag"], 1, "{lagging}");
    let packet = f.context("where is beta defined");
    assert!(packet["evidence"].as_array().is_some_and(|e| !e.is_empty()));
    assert_eq!(packet["mechanisms"]["code_graph_searched"], true);
    // An episode recorded without its source manifest (an older store): the
    // arm is skipped and the lag is unknown. A query reads the stored
    // manifest only; it never searches the retained sync manifests for one.
    f.store
        .connect()
        .unwrap()
        .execute(
            "UPDATE gf_episodes SET attributes=json_remove(attributes,'$.source_heads') WHERE repository_id=?",
            [&f.repo],
        )
        .unwrap();
    let unknown = status(&f);
    assert_eq!(unknown["status"], "no_episode", "{unknown}");
    assert_eq!(unknown["latest_episode"]["episode"], projected["episode"]);
    assert!(unknown["episode_lag"].is_null(), "{unknown}");
}

/// A repository that was never synced has no head files and
/// nothing queued.
#[test]
fn an_unsynced_repository_reports_no_projection() {
    let dir = TempDir::new().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let ws = store.create_workspace("Unsynced").unwrap();
    let (workspace, key) = (s(&ws, "id"), s(&ws, "capability"));
    let repo = store
        .register(
            workspace,
            key,
            &json!({"project_key":"empty","name":"Empty"}),
        )
        .unwrap();
    let packet = store
        .project_context(
            workspace,
            key,
            &json!({"repository_id":repo["id"],"question":"anything"}),
        )
        .unwrap();
    let skipped = &packet["code_graph"];
    assert_eq!(skipped["status"], "no_episode", "{skipped}");
    assert_eq!(skipped["projection"], "none", "{skipped}");
    assert!(skipped["projection_job"].is_null());
    assert_eq!(skipped["head_files"], 0);
    assert!(skipped["latest_episode"].is_null());
    assert!(skipped["episode_lag"].is_null());
}

/// `context` writes nothing, with the skip
/// reasons and guards as before: another connection's `PRAGMA data_version`
/// does not move across a query, whether the arm is skipped or fused.
#[test]
fn context_opens_no_write_transaction() {
    let f = Fixture::new(&[("a.py", A_V1), ("b.py", B)]);
    let watcher = f.store.connect().unwrap();
    let version = || -> i64 {
        watcher
            .query_row("PRAGMA data_version", [], |r| r.get(0))
            .unwrap()
    };
    let before = version();
    assert_eq!(
        f.context("where is beta defined")["code_graph"]["status"],
        "no_episode"
    );
    assert_eq!(version(), before, "a skipped-arm query wrote to the store");
    f.project();
    let before = version();
    assert_eq!(
        f.context("where is beta defined")["code_graph"]["status"],
        "fused"
    );
    assert_eq!(version(), before, "a fused-arm query wrote to the store");
}

/// `mechanisms.memories_withheld.by_file` counts the reviewed
/// memories a changed file withholds. A member note that depends on `a.py`
/// is served while `a.py` is unchanged and withheld, and counted, once it
/// changes.
#[test]
fn a_changed_file_withholds_its_reviewed_memory_and_says_so() {
    let f = Fixture::new(&[("a.py", A_V1), ("b.py", B)]);
    f.store
        .note(
            &f.workspace,
            &f.key,
            &f.args(json!({
                "title": "alpha returns one",
                "body": "alpha() in a.py returns 1, and run() relies on it.",
                "dependencies": ["a.py"],
            })),
        )
        .unwrap();
    let fresh = f.context("what does alpha return");
    assert_eq!(
        fresh["mechanisms"]["memories_withheld"],
        json!({"by_file":0,"by_symbol":0}),
        "{}",
        fresh["mechanisms"]
    );
    assert_eq!(fresh["coverage"]["stale_memories"], 0);
    f.sync(&[
        (
            "a.py",
            "def alpha():\n    return 10\n\ndef beta():\n    return 2\n",
        ),
        ("b.py", B),
    ]);
    let changed = f.context("what does alpha return");
    assert_eq!(
        changed["mechanisms"]["memories_withheld"],
        json!({"by_file":1,"by_symbol":0}),
        "{}",
        changed["mechanisms"]
    );
    assert_eq!(changed["coverage"]["stale_memories"], 1);
}

/// `mechanisms.graph_local_status` per value of the
/// `graph_retrieval` switch: `false` gives `disabled`, `true` gives
/// `searched`, and no value, without a current reviewed memory, gives
/// `not_requested`.
#[test]
fn graph_local_status_follows_the_switch() {
    let f = Fixture::new(&[("a.py", A_V1), ("b.py", B)]);
    let status = |switch: Option<bool>| {
        let mut args = f.args(json!({"question":"where is beta defined"}));
        if let Some(on) = switch {
            args["graph_retrieval"] = json!(on);
        }
        let packet = f
            .store
            .project_context(&f.workspace, &f.key, &args)
            .unwrap();
        assert!(
            packet["mechanisms"].get("graph_local_degraded").is_none(),
            "{}",
            packet["mechanisms"]
        );
        packet["mechanisms"]["graph_local_status"].clone()
    };
    assert_eq!(status(Some(false)), "disabled");
    assert_eq!(status(Some(true)), "searched");
    assert_eq!(status(None), "not_requested");
}

/// Through the store: a recorded episode whose call sites give
/// more than 50,000 cross-file static edges no longer fails `context`. The
/// 60,000 call edges from `b.py` to `a.py` fold to one file pair, and the
/// packet says so.
#[test]
fn a_recorded_episode_past_the_static_edge_bound_folds_its_edges() {
    let f = Fixture::new(&[("a.py", A_V1), ("b.py", B)]);
    let calls = 60_000;
    let links: Vec<Value> = (1..=calls)
        .map(|line| {
            json!({"source":"b_run","target":"a_alpha","relation":"calls","key":format!("calls-{line}"),
                "source_location":format!("L{line}"),"confidence":"EXTRACTED"})
        })
        .collect();
    let graph = graphicle::graph::Graph::from_node_link(
        &json!({
            "directed": true, "multigraph": true,
            "nodes": [
                {"id":"a","label":"a.py","kind":"file","relpath":"a.py","source_file":"a.py","source_location":"L1"},
                {"id":"b","label":"b.py","kind":"file","relpath":"b.py","source_file":"b.py","source_location":"L1"},
                {"id":"a_alpha","label":"alpha()","kind":"function","relpath":"a.py","source_file":"a.py","source_location":"L1"},
                {"id":"b_run","label":"run()","kind":"function","relpath":"b.py","source_file":"b.py","source_location":"L3"}
            ],
            "links": links,
        }),
        "links",
    )
    .unwrap();
    let db = f.store.connect().unwrap();
    let episode = graphicle_runtime::graph_projection::current_episode(&db, &f.repo).unwrap();
    assert!(graphicle_runtime::graph_projection::persist(&db, &f.repo, &episode, &graph).unwrap());
    let packet = f.context("where is alpha called");
    assert_eq!(
        packet["graph_source"], "episode",
        "{}",
        packet["mechanisms"]
    );
    let mechanisms = &packet["mechanisms"];
    assert_eq!(
        mechanisms["static_relationship_edges"], calls,
        "{mechanisms}"
    );
    assert_eq!(mechanisms["static_edges_aggregated"], true);
    assert_eq!(mechanisms["static_edges_capped"], false);
    assert_eq!(
        mechanisms["static_edges_bound"],
        json!({"received":calls,"distinct":1,"used":1,"bound":50_000,"aggregated":true,
            "capped":false,"lift_complete":true})
    );
    assert!(packet["evidence"].as_array().is_some_and(|e| !e.is_empty()));
}
