//! Private memory per credential: a credential made with a
//! `memory_namespace` reads the shared project as any credential does, and
//! reads and writes only its own namespace's memory.
//!
//! These tests prove isolation by behaviour, not by construction: credential
//! B is pointed at everything credential A wrote (A's memory ids, its claim
//! ids, its job id, its work-state text, the words of its memory) through
//! every read path, over the store, the command API and MCP, and every reply
//! B gets is searched for any trace of A. A last test reads this crate's SQL
//! and fails on a statement that touches a namespaced table without the
//! namespace predicate, unless the allowlist below gives the reason.
//!
//! With `GRAPHICLE_DATABASE_URL` set, `Store::open` makes a PostgreSQL store
//! and every test here runs on it.
use graphicle_runtime::{
    api::{self, App},
    jobs, model,
    store::{Store, s},
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tempfile::TempDir;

/// A's secret: in its memory title and body, its work state, its claim.
const MARK: &str = "ZEBRA7731";
const NOTES: &str = "Deploys go through the blue pipeline.\nRollbacks need two approvals.\n";
const CODE: &str = "def deploy(target):\n    return pipeline(target)\n\n\ndef pipeline(target):\n    return target\n";

struct Fixture {
    _dir: TempDir,
    store: Store,
    workspace: String,
    owner: String,
    repo: String,
}

struct Key {
    capability: String,
}

impl Fixture {
    fn new() -> Self {
        crate::edition_setup();
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let ws = store.create_workspace("Namespaces").unwrap();
        let workspace = s(&ws, "id").to_string();
        let owner = s(&ws, "capability").to_string();
        let repo = s(
            &store
                .register(&workspace, &owner, &json!({"project_key":"ns","name":"ns"}))
                .unwrap(),
            "id",
        )
        .to_string();
        let f = Self {
            _dir: dir,
            store,
            workspace,
            owner,
            repo,
        };
        f.sync(&[("notes.txt", NOTES), ("deploy.py", CODE)]);
        jobs::drain_projection(&f.store, &f.repo).unwrap();
        f
    }
    fn args(&self, mut value: Value) -> Value {
        if value.get("repository_id").is_none() {
            value["repository_id"] = json!(self.repo);
        }
        value
    }
    fn sync(&self, files: &[(&str, &str)]) {
        let state = self
            .store
            .project_status(&self.workspace, &self.owner, &self.args(json!({})))
            .unwrap();
        let manifest: Vec<Value> = files
            .iter()
            .map(|(p, t)| json!({"path":p,"sha256":graphicle_memory::text::sha256(t.as_bytes()),"bytes":t.len()}))
            .collect();
        let begin = self
            .store
            .sync_prepare(
                &self.workspace,
                &self.owner,
                &self.args(json!({"expected_snapshot":state["repository"]["snapshot"],"manifest":manifest})),
            )
            .unwrap();
        let upload: Vec<Value> = files
            .iter()
            .map(|(p, t)| json!({"path":p,"content":t}))
            .collect();
        self.store
            .sync_upload(
                &self.workspace,
                &self.owner,
                &self.args(json!({"sync_id":begin["sync_id"],"files":upload})),
            )
            .unwrap();
        self.store
            .sync_commit(
                &self.workspace,
                &self.owner,
                &self.args(json!({"sync_id":begin["sync_id"]})),
            )
            .unwrap();
    }
    /// A credential made by the owner API graphicle.dev's gate calls.
    fn key(&self, role: &str, namespace: Option<&str>, self_review: Option<bool>) -> Key {
        let mut args = json!({"role":role,"label":format!("{role} {namespace:?}")});
        if let Some(namespace) = namespace {
            args["memory_namespace"] = json!(namespace);
        }
        if let Some(review) = self_review {
            args["self_review"] = json!(review);
        }
        let made = self
            .store
            .grant(&self.workspace, &self.owner, &args)
            .unwrap();
        assert_eq!(made["memory_namespace"], json!(namespace));
        Key {
            capability: s(&made, "capability").to_string(),
        }
    }
    fn app(&self) -> App {
        let mut app = App::new(self.store.clone(), model::Config::default(), vec![]).unwrap();
        app.mcp_learning = true;
        app.mcp_work_state = true;
        app
    }
    fn command(&self, key: &Key, operation: &str, args: Value) -> graphicle_runtime::Result<Value> {
        api::command(
            &self.app(),
            &self.workspace,
            &key.capability,
            operation,
            &self.args(args),
        )
    }
    fn tool(&self, key: &Key, name: &str, arguments: Value) -> Value {
        api::mcp(
            &self.app(),
            &self.workspace,
            &key.capability,
            &self.repo,
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments}}),
        )
        .unwrap()
    }
    /// What A writes: a proposed memory it approves itself, work state, and
    /// an interpreted claim it approves. Returns the ids B will probe.
    fn write_as(&self, key: &Key, tag: &str) -> BTreeMap<&'static str, String> {
        let mut ids = BTreeMap::new();
        let proposed = self
            .command(
                key,
                "memory.propose",
                json!({"kind":"decision","title":format!("{MARK} {tag} deploy rule"),
                    "body":format!("{MARK} {tag}: deploys use the blue pipeline."),
                    "idempotency_key":format!("{tag}-1"),
                    "evidence":[{"source_id":"file:notes.txt","quote":"Deploys go through the blue pipeline."}]}),
            )
            .unwrap();
        let memory = s(&proposed, "memory_id").to_string();
        self.command(
            key,
            "learning.review",
            json!({"memory_id":memory,"expected_revision":1,"decision":"approve",
                "maintainer":"self","reason":"self review"}),
        )
        .unwrap();
        ids.insert("memory", memory);
        let written = self
            .command(
                key,
                "work_state.write",
                json!({"list":format!("{MARK} {tag} list"),"fields":{"task":format!("{MARK} {tag} ship it"),"status":"doing"}}),
            )
            .unwrap();
        ids.insert("work_state", written["entry_id"].to_string());
        let prepared = self
            .command(
                key,
                "interpretation.prepare",
                json!({"source_id":"file:notes.txt"}),
            )
            .unwrap();
        let statement = "Rollbacks need two approvals.";
        let claim = json!({"statement":statement,"subject":format!("{MARK} {tag} rollbacks"),"relation":"need","object":"two approvals",
            "speaker":"document","modality":"requirement","conditions":[],"action":"ADD","target_id":null,
            "valid_at":null,"invalid_at":null,"time_basis":"unknown","anchor_id":null,
            "evidence":[{"id":"file:notes.txt","quote":statement}]});
        let proposed = self
            .command(
                key,
                "interpretation.propose",
                json!({"source_id":"file:notes.txt","context_sha256":prepared["sha256"],"claims":[claim]}),
            )
            .unwrap();
        let draft = s(&proposed["drafts"][0], "memory_id").to_string();
        self.command(
            key,
            "learning.review",
            json!({"memory_id":draft,"expected_revision":1,"decision":"approve","maintainer":"self","reason":"self review"}),
        )
        .unwrap();
        ids.insert("claim_memory", draft);
        let claims = self.command(key, "claim.history", json!({})).unwrap();
        let claim_id = claims["claims"]
            .as_array()
            .and_then(|c| c.first())
            .map(|c| s(c, "id").to_string())
            .unwrap_or_default();
        assert!(!claim_id.is_empty(), "A has a claim: {claims:#}");
        ids.insert("claim", claim_id);
        ids
    }
    /// Recall over the command API and MCP, as one answer to compare.
    fn recall(&self, key: &Key, question: &str) -> Value {
        let context = self
            .command(key, "context", json!({"question":question}))
            .unwrap();
        let mcp = self.tool(
            key,
            "recall_project",
            json!({"question":question,"structured":true}),
        );
        json!({"context":context["memories"],"mcp":mcp["result"]["structuredContent"]["memories"]})
    }
}

/// Every trace of A's writing that B must never get back.
fn traces(ids: &BTreeMap<&'static str, String>) -> Vec<String> {
    let mut out: Vec<String> = ids
        .iter()
        .filter(|(kind, _)| **kind != "work_state")
        .map(|(_, id)| id.clone())
        .collect();
    out.push(MARK.to_string());
    // Nor B's own namespace: replies to a namespaced key are a shared key's.
    out.push("memory_namespace".to_string());
    out
}

fn assert_clean(label: &str, reply: &Value, forbidden: &[String]) {
    let text = reply.to_string();
    for trace in forbidden {
        assert!(
            !text.contains(trace.as_str()),
            "{label}: B's reply carries A's {trace}: {reply:#}"
        );
    }
}

/// B's answer for an id of A's must be exactly the answer for an id nobody
/// has: the same status and the same message.
fn assert_unavailable(
    label: &str,
    of_a: graphicle_runtime::Result<Value>,
    unknown: graphicle_runtime::Result<Value>,
) {
    let a = of_a.expect_err(&format!("{label}: B reached A's row"));
    let u = unknown.expect_err(&format!("{label}: unknown id answered"));
    assert_eq!((a.status, &a.message), (u.status, &u.message), "{label}");
}

/// Everything B can read, through every read path, pointed at A's ids.
fn probe_all(f: &Fixture, b: &Key, a: &BTreeMap<&'static str, String>) {
    let forbidden = traces(a);
    // B asks in A's words, never with A's marker: a reply echoes its question.
    let question = "deploy rule blue pipeline rollbacks approvals".to_string();
    let memory = &a["memory"];
    let reads: Vec<(&str, graphicle_runtime::Result<Value>)> = vec![
        (
            "memories",
            f.command(b, "memories", json!({"include_retired":true})),
        ),
        (
            "memory.search",
            f.command(b, "memory.search", json!({"question":question})),
        ),
        (
            "memory.search lexical",
            f.command(
                b,
                "memory.search",
                json!({"question":"blue pipeline","match_mode":"lexical"}),
            ),
        ),
        ("memory.export", f.command(b, "memory.export", json!({}))),
        (
            "memory.template",
            f.command(b, "memory.template", json!({})),
        ),
        (
            "work_state.read",
            f.command(b, "work_state.read", json!({"include_closed":true})),
        ),
        (
            "context",
            f.command(b, "context", json!({"question":question})),
        ),
        ("status", f.command(b, "status", json!({}))),
        ("claim.history", f.command(b, "claim.history", json!({}))),
        (
            "workflow.status",
            f.command(b, "workflow.status", json!({})),
        ),
        ("pack", f.command(b, "pack", json!({"question":question}))),
        (
            "interpretation.prepare",
            f.command(
                b,
                "interpretation.prepare",
                json!({"source_id":"file:notes.txt"}),
            ),
        ),
    ];
    for (label, reply) in reads {
        let reply = reply.unwrap_or_else(|e| panic!("{label}: {}", e.message));
        assert_clean(label, &reply, &forbidden);
    }
    let listed = f
        .command(b, "memories", json!({"include_retired":true}))
        .unwrap();
    assert_eq!(listed, json!([]), "B has written nothing");
    let status = f.command(b, "status", json!({})).unwrap();
    assert_eq!(status["memories"], 0, "no count of A's memory: {status:#}");
    assert_eq!(status["draft_memories"], 0);

    // A's ids get the answer an unknown id gets.
    let history = |id: &str| f.command(b, "memory.history", json!({"memory_id":id}));
    assert_unavailable("memory.history", history(memory), history("no-such-memory"));
    let review = |id: &str| {
        f.command(b, "learning.review", json!({"memory_id":id,"expected_revision":1,"decision":"reject","maintainer":"b","reason":"probe"}))
    };
    assert_unavailable("learning.review", review(memory), review("no-such-memory"));
    let delete = |id: &str| f.command(b, "memory.delete", json!({"memory_id":id}));
    assert_unavailable("memory.delete", delete(memory), delete("no-such-memory"));
    let refresh = |id: &str| f.command(b, "learning.refresh.prepare", json!({"memory_id":id}));
    assert_unavailable(
        "learning.refresh.prepare",
        refresh(memory),
        refresh("no-such-memory"),
    );
    let as_of = |id: &str| {
        f.command(
            b,
            "memory.history",
            json!({"memory_id":id,"known_at":"2099-01-01T00:00:00Z"}),
        )
    };
    assert_unavailable(
        "memory.history known_at",
        as_of(memory),
        as_of("no-such-memory"),
    );
    // B cannot write into A's memory with A's id or idempotency key.
    let edit = |id: &str| {
        f.command(
            b,
            "note",
            json!({"memory_id":id,"expected_revision":2,"title":"B edit","body":"B overwrites."}),
        )
    };
    assert_unavailable("note edit", edit(memory), edit("no-such-memory"));
    let replay = f
        .command(
            b,
            "memory.propose",
            json!({"kind":"decision","title":format!("{MARK} A deploy rule"),
                "body":format!("{MARK} A: deploys use the blue pipeline."),"idempotency_key":"A-1",
                "evidence":[{"source_id":"file:notes.txt","quote":"Deploys go through the blue pipeline."}]}),
        )
        .unwrap();
    assert_ne!(
        replay["replayed"], true,
        "B replayed A's proposal: {replay:#}"
    );
    assert_ne!(s(&replay, "memory_id"), memory.as_str());
    // That draft is B's own; clean it so later probes see B empty again.
    f.command(b, "memory.delete", json!({"memory_id":replay["memory_id"]}))
        .unwrap();

    // MCP: the tools an agent of B calls.
    let app_reads = [
        (
            "recall_project",
            json!({"question":question,"structured":true}),
        ),
        ("recall_project", json!({"question":question})),
        ("project_memory", json!({"include_retired":true})),
        ("search_project_memory", json!({"question":question})),
        ("project_memory_history", json!({"memory_id":memory})),
        ("repository_status", json!({})),
        ("workflow_status", json!({})),
        ("work_state_read", json!({"include_closed":true})),
        ("prepare_memory_refresh", json!({"memory_id":memory})),
        (
            "prepare_observations",
            json!({"source_id":"file:notes.txt"}),
        ),
    ];
    for (name, arguments) in app_reads {
        let reply = f.tool(b, name, arguments);
        assert_clean(name, &reply, &forbidden);
    }
    // The session-start section an agent of B gets on connect.
    let initialized = api::mcp(
        &f.app(),
        &f.workspace,
        &b.capability,
        &f.repo,
        &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"probe","version":"1"}}}),
    )
    .unwrap();
    assert_clean("initialize", &initialized, &forbidden);
}

#[test]
fn two_namespaces_on_one_project_never_see_each_others_memory() {
    let f = Fixture::new();
    let a = f.key("agent", Some("dev-a"), Some(true));
    let b = f.key("agent", Some("dev-b"), Some(true));
    let written = f.write_as(&a, "A");

    // A sees its own memory, work state and claim.
    let own = f.command(&a, "memories", json!({})).unwrap();
    assert_eq!(own.as_array().unwrap().len(), 2, "{own:#}");
    assert!(
        f.recall(&a, "deploy rule blue pipeline")
            .to_string()
            .contains(MARK)
    );
    assert!(
        f.command(&a, "work_state.read", json!({}))
            .unwrap()
            .to_string()
            .contains(MARK)
    );

    // B: nothing, by any path, and B's own recall twice (a cached answer of
    // A's must not come back to B either).
    probe_all(&f, &b, &written);
    probe_all(&f, &b, &written);

    // B's writing is equally invisible to A, by the same probes.
    let b_written = f.write_as(&b, "B");
    let b_traces: Vec<String> = b_written
        .iter()
        .filter(|(kind, _)| **kind != "work_state")
        .map(|(_, id)| id.clone())
        .chain([format!("{MARK} B")])
        .collect();
    for (label, reply) in [
        (
            "memories",
            f.command(&a, "memories", json!({"include_retired":true}))
                .unwrap(),
        ),
        ("recall", f.recall(&a, "B deploy rule blue pipeline")),
        (
            "work_state.read",
            f.command(&a, "work_state.read", json!({"include_closed":true}))
                .unwrap(),
        ),
        (
            "claim.history",
            f.command(&a, "claim.history", json!({})).unwrap(),
        ),
        ("status", f.command(&a, "status", json!({})).unwrap()),
    ] {
        assert_clean(label, &reply, &b_traces);
    }
}

#[test]
fn a_rotated_key_in_the_same_namespace_sees_that_namespace() {
    let f = Fixture::new();
    let a = f.key("agent", Some("dev-a"), Some(true));
    let written = f.write_as(&a, "A");
    // The owner made the namespace with self_review; a new key may name it
    // without restating the policy, and may not change it.
    let refused = f.store.grant(
        &f.workspace,
        &f.owner,
        &json!({"role":"agent","label":"flip","memory_namespace":"dev-a","self_review":false}),
    );
    assert_eq!(refused.unwrap_err().status, 409);
    let rotated = f.key("agent", Some("dev-a"), None);
    let listed = f.command(&rotated, "memories", json!({})).unwrap();
    assert!(
        listed.to_string().contains(&written["memory"]),
        "{listed:#}"
    );
    assert!(
        f.recall(&rotated, "deploy rule blue pipeline")
            .to_string()
            .contains(MARK)
    );
    assert!(
        f.command(&rotated, "work_state.read", json!({}))
            .unwrap()
            .to_string()
            .contains(MARK)
    );
    let history = f
        .command(
            &rotated,
            "memory.history",
            json!({"memory_id":written["memory"]}),
        )
        .unwrap();
    assert_eq!(history["versions"].as_array().unwrap().len(), 2);
}

#[test]
fn keys_without_a_namespace_keep_shared_memory_and_never_see_a_namespace() {
    let f = Fixture::new();
    // Shared memory, written and reviewed as before namespaces existed.
    let shared_agent = f.key("agent", None, None);
    let reviewer = f.key("reviewer", None, None);
    let proposed = f
        .command(
            &shared_agent,
            "memory.propose",
            json!({"kind":"decision","title":"Shared deploy rule","body":"Everyone: deploys use the blue pipeline.",
                "idempotency_key":"shared-1","evidence":[{"source_id":"file:notes.txt","quote":"Deploys go through the blue pipeline."}]}),
        )
        .unwrap();
    // A shared agent still cannot approve its own draft.
    let own = f.command(
        &shared_agent,
        "learning.review",
        json!({"memory_id":proposed["memory_id"],"expected_revision":1,"decision":"approve","maintainer":"x","reason":"x"}),
    );
    assert_eq!(own.unwrap_err().status, 403);
    f.command(
        &reviewer,
        "learning.review",
        json!({"memory_id":proposed["memory_id"],"expected_revision":1,"decision":"approve","maintainer":"r","reason":"checked"}),
    )
    .unwrap();
    // self_review is refused without a namespace.
    assert_eq!(
        f.store
            .grant(
                &f.workspace,
                &f.owner,
                &json!({"role":"agent","label":"x","self_review":true})
            )
            .unwrap_err()
            .status,
        400
    );
    // A namespace without self_review: its agent cannot approve.
    let strict = f.key("agent", Some("dev-strict"), None);
    let draft = f
        .command(
            &strict,
            "memory.propose",
            json!({"kind":"note","title":"Strict draft","body":"A draft nobody approves.","idempotency_key":"s-1",
                "evidence":[{"source_id":"file:notes.txt","quote":"Rollbacks need two approvals."}]}),
        )
        .unwrap();
    assert_eq!(
        f.command(&strict, "learning.review", json!({"memory_id":draft["memory_id"],"expected_revision":1,"decision":"approve","maintainer":"x","reason":"x"}))
            .unwrap_err()
            .status,
        403
    );

    let a = f.key("agent", Some("dev-a"), Some(true));
    let written = f.write_as(&a, "A");
    // Shared keys and the owner: shared memory exactly as before, none of A's.
    for key in [
        &shared_agent,
        &reviewer,
        &Key {
            capability: f.owner.clone(),
        },
    ] {
        let listed = f.command(key, "memories", json!({})).unwrap();
        let titles: Vec<&str> = listed
            .as_array()
            .unwrap()
            .iter()
            .map(|m| s(m, "title"))
            .collect();
        assert_eq!(titles, ["Shared deploy rule"]);
        assert_clean("shared memories", &listed, &traces(&written));
        let recalled = f.recall(key, "deploy rule blue pipeline");
        assert!(
            recalled.to_string().contains("Shared deploy rule"),
            "{recalled:#}"
        );
        assert_clean("shared recall", &recalled, &traces(&written));
        assert_clean(
            "shared work state",
            &f.command(key, "work_state.read", json!({"include_closed":true}))
                .unwrap(),
            &traces(&written),
        );
        assert_clean(
            "shared claims",
            &f.command(key, "claim.history", json!({})).unwrap(),
            &traces(&written),
        );
    }
    // The tool contract is the same for every key: a namespaced key lists
    // exactly the tools, descriptions and schemas a shared key of its role
    // lists.
    let listed = |key: &Key| -> Value {
        api::mcp(
            &f.app(),
            &f.workspace,
            &key.capability,
            &f.repo,
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}),
        )
        .unwrap()
    };
    assert_eq!(listed(&shared_agent), listed(&a));
    // And a namespaced key never sees shared memory.
    let none = f.command(&a, "memories", json!({})).unwrap();
    assert!(!none.to_string().contains("Shared deploy rule"), "{none:#}");
    assert!(
        !f.recall(&a, "deploy rule blue pipeline")
            .to_string()
            .contains("Shared deploy rule")
    );
}

#[test]
fn export_delete_import_restores_the_same_recall() {
    let f = Fixture::new();
    let a = f.key("agent", Some("dev-a"), Some(true));
    let b = f.key("agent", Some("dev-b"), Some(true));
    let written = f.write_as(&a, "A");
    f.write_as(&b, "B");
    let question = "deploy rule blue pipeline";
    let before = f.recall(&a, question);
    let work_before = f
        .command(&a, "work_state.read", json!({"include_closed":true}))
        .unwrap();
    let b_before = f.recall(&b, question);

    // Only the owner runs namespace operations.
    assert_eq!(
        f.store
            .namespace_export(&f.workspace, &a.capability, "dev-a")
            .unwrap_err()
            .status,
        403
    );
    let exported = f
        .store
        .namespace_export(&f.workspace, &f.owner, "dev-a")
        .unwrap();
    assert!(exported["rows"].as_u64().unwrap() > 0);
    assert_eq!(exported["sha256"].as_str().unwrap().len(), 64);
    assert!(
        !exported.to_string().contains(&format!("{MARK} B")),
        "B's rows in A's export"
    );

    let deleted = f
        .store
        .namespace_delete(&f.workspace, &f.owner, "dev-a")
        .unwrap();
    assert_eq!(deleted["remaining"], 0, "{deleted:#}");
    assert!(deleted["receipt"].is_object(), "{deleted:#}");
    // The residue scan, independently: nothing of dev-a in any table.
    let stats = f
        .store
        .namespace_stats(&f.workspace, &f.owner, "dev-a")
        .unwrap();
    assert_eq!(stats["rows"], 0, "{stats:#}");
    assert_eq!(
        f.command(&a, "memories", json!({"include_retired":true}))
            .unwrap(),
        json!([])
    );
    assert!(!f.recall(&a, question).to_string().contains(MARK));
    // B is untouched.
    assert_eq!(f.recall(&b, question), b_before);

    // A tampered archive is refused; the real one restores.
    let mut tampered = exported.clone();
    tampered["archive"]["tables"]["repo_memories"][0]["body"] = json!("forged");
    assert!(
        f.store
            .namespace_import(&f.workspace, &f.owner, "dev-a", &tampered)
            .is_err()
    );
    // An archive of dev-a cannot be restored into dev-b.
    assert!(
        f.store
            .namespace_import(&f.workspace, &f.owner, "dev-b", &exported)
            .is_err()
    );
    let restored = f
        .store
        .namespace_import(&f.workspace, &f.owner, "dev-a", &exported)
        .unwrap();
    assert_eq!(restored["restored"], exported["rows"]);
    assert_eq!(f.recall(&a, question), before);
    assert_eq!(
        f.command(&a, "work_state.read", json!({"include_closed":true}))
            .unwrap()["text"],
        work_before["text"]
    );
    assert!(
        f.command(&a, "memories", json!({}))
            .unwrap()
            .to_string()
            .contains(&written["memory"])
    );
    // A second import into a namespace that holds rows is a conflict.
    assert_eq!(
        f.store
            .namespace_import(&f.workspace, &f.owner, "dev-a", &exported)
            .unwrap_err()
            .status,
        409
    );
}

#[test]
fn deleting_the_project_removes_namespaced_memory_and_its_caches() {
    let f = Fixture::new();
    let a = f.key("agent", Some("dev-a"), Some(true));
    f.write_as(&a, "A");
    f.recall(&a, "deploy rule blue pipeline");
    assert!(
        f.store
            .namespace_stats(&f.workspace, &f.owner, "dev-a")
            .unwrap()["rows"]
            .as_u64()
            .unwrap()
            > 0
    );
    let snapshot = f
        .store
        .project_status(&f.workspace, &f.owner, &f.args(json!({})))
        .unwrap()["repository"]["snapshot"]
        .clone();
    api::command(
        &f.app(),
        &f.workspace,
        &f.owner,
        "project.delete",
        &json!({"repository_id":f.repo,"expected_snapshot":snapshot}),
    )
    .unwrap();
    let stats = f
        .store
        .namespace_stats(&f.workspace, &f.owner, "dev-a")
        .unwrap();
    assert_eq!(stats["rows"], 0, "{stats:#}");
    let db = f.store.connect().unwrap();
    for &(table, _) in graphicle_runtime::memory_scope::TABLES {
        let count: i64 = db
            .query_row(
                &format!("SELECT count(*) FROM {table} WHERE memory_namespace='dev-a'"),
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        assert_eq!(
            count, 0,
            "{table} keeps dev-a rows after the project was deleted"
        );
    }
}

#[test]
fn stats_report_the_last_read_and_write() {
    let f = Fixture::new();
    let a = f.key("agent", Some("dev-a"), Some(true));
    let fresh = f
        .store
        .namespace_stats(&f.workspace, &f.owner, "dev-a")
        .unwrap();
    assert!(
        fresh["last_write_at"].is_null() && fresh["last_read_at"].is_null(),
        "{fresh:#}"
    );
    f.command(
        &a,
        "work_state.write",
        json!({"list":"todo","fields":{"task":"one","status":"todo"}}),
    )
    .unwrap();
    let written = f
        .store
        .namespace_stats(&f.workspace, &f.owner, "dev-a")
        .unwrap();
    let wrote = s(&written, "last_write_at").to_string();
    assert!(!wrote.is_empty(), "{written:#}");
    assert!(written["rows"].as_u64().unwrap() >= 1 && written["bytes"].as_u64().unwrap() > 0);
    std::thread::sleep(std::time::Duration::from_millis(5));
    f.recall(&a, "deploy");
    let read = f
        .store
        .namespace_stats(&f.workspace, &f.owner, "dev-a")
        .unwrap();
    assert!(!s(&read, "last_read_at").is_empty(), "{read:#}");
    assert!(s(&read, "last_read_at") >= wrote.as_str());
    // A recall is a read, not a write.
    assert_eq!(s(&read, "last_write_at"), wrote);
    // Unknown and invalid names are refused alike, and only the owner asks.
    assert!(
        f.store
            .namespace_stats(&f.workspace, &f.owner, "nobody")
            .is_err()
    );
    assert!(
        f.store
            .namespace_stats(&f.workspace, &f.owner, "' OR 1=1")
            .is_err()
    );
    assert_eq!(
        f.store
            .namespace_stats(&f.workspace, &a.capability, "dev-a")
            .unwrap_err()
            .status,
        403
    );
}

#[test]
fn a_namespace_name_is_validated_and_the_column_is_immutable_data() {
    let f = Fixture::new();
    for bad in [
        "",
        "a b",
        "x'; DROP TABLE credentials; --",
        &"n".repeat(129),
    ] {
        let refused = f.store.grant(
            &f.workspace,
            &f.owner,
            &json!({"role":"agent","label":"bad","memory_namespace":bad}),
        );
        assert_eq!(refused.unwrap_err().status, 400, "{bad:?}");
    }
    assert!(
        f.store
            .grant(
                &f.workspace,
                &f.owner,
                &json!({"role":"agent","label":"bad","memory_namespace":7})
            )
            .is_err()
    );
}

/// Every SQL statement of this crate that names a table holding namespaced
/// memory must carry the namespace predicate, or be listed here with the
/// number of such statements in that file and why each is safe. A new
/// unscoped statement fails this test until someone reviews it.
#[test]
fn every_sql_statement_on_namespaced_tables_is_scoped() {
    // (file, table, statements, why they need no namespace predicate)
    const JOB_QUEUE: &str = "worker internals: lease, heartbeat, finish, backoff and budget accounting of a job by its id, which only the worker holds; no caller-supplied id reaches them";
    const OWNER_ONLY: &str = "owner/admin operations over every namespace (maintenance, usage, metering, revocation, connectors, vault); a namespaced credential cannot call them";
    const WHOLE_PROJECT: &str = "removes a project's caches or jobs wholesale on a source, permission or project change: every namespace's rows go, which is what a shared change requires";
    const FORGET_SOURCE: &str = "certified forgetting of a source: every namespace's memory and derivations citing the forgotten documents go, and the receipt counts them all";
    const SCOPED_PARENT: &str =
        "updates a row by the id a namespace-scoped query of the same transaction just returned";
    // In deletion.rs "memory.delete" statements count, delete and check for
    // residue by the ids the caller's scoped memory listing returned in the
    // same transaction.
    const ALLOW: &[(&str, &str, usize, &str)] = &[
        ("auto_observe.rs", "am_jobs", 2, OWNER_ONLY),
        ("claims.rs", "am_claims", 3, SCOPED_PARENT),
        ("connectors.rs", "am_jobs", 3, OWNER_ONLY),
        ("connectors.rs", "am_model_cache", 2, WHOLE_PROJECT),
        (
            "deletion.rs",
            "am_claims",
            3,
            "memory.delete by scoped ids (2), and certified forgetting of a source (1)",
        ),
        ("deletion.rs", "am_document_reads", 2, FORGET_SOURCE),
        (
            "deletion.rs",
            "am_entities",
            1,
            "certified forgetting of a source: entities no claim links any more, in every namespace",
        ),
        (
            "deletion.rs",
            "am_entity_links",
            1,
            "certified forgetting of a source: entities no claim links any more, in every namespace",
        ),
        (
            "deletion.rs",
            "am_vectors",
            5,
            "memory.delete by scoped ids (2), and certified forgetting of a source (3)",
        ),
        (
            "deletion.rs",
            "repo_memories",
            7,
            "memory.delete by scoped ids (2), certified forgetting of a source (3), and the owner's deletion.state digest and deletion.verify (2)",
        ),
        (
            "deletion.rs",
            "repo_memory_revisions",
            3,
            "memory.delete by scoped ids (2), and certified forgetting of a source (1)",
        ),
        ("doc_links.rs", "am_jobs", 4, JOB_QUEUE),
        (
            "ingestion.rs",
            "am_jobs",
            1,
            "the per-project queue byte bound, a total over every namespace that returns no row",
        ),
        ("jobs.rs", "am_call_reservations", 9, JOB_QUEUE),
        ("jobs.rs", "am_jobs", 29, JOB_QUEUE),
        ("jobs.rs", "am_model_cache", 1, JOB_QUEUE),
        ("jobs.rs", "am_model_calls", 6, JOB_QUEUE),
        ("maintenance.rs", "am_jobs", 2, OWNER_ONLY),
        ("maintenance.rs", "am_model_cache", 1, OWNER_ONLY),
        ("maintenance.rs", "repo_memories", 1, OWNER_ONLY),
        ("maintenance.rs", "repo_memory_revisions", 1, OWNER_ONLY),
        ("maintenance.rs", "repo_runs", 1, OWNER_ONLY),
        (
            "memory.rs",
            "am_claims",
            1,
            "reviewed-state digest: entity links joined to claims, kept only for memory ids the scoped listing returned",
        ),
        (
            "memory.rs",
            "am_entity_links",
            1,
            "reviewed-state digest: entity links joined to claims, kept only for memory ids the scoped listing returned",
        ),
        ("metering.rs", "am_jobs", 1, OWNER_ONLY),
        ("metering.rs", "am_model_calls", 2, OWNER_ONLY),
        (
            "namespace_owner.rs",
            "am_jobs",
            1,
            "owner namespace operations: the parent subquery of a predicate whose outer table is filtered by namespace",
        ),
        (
            "namespace_owner.rs",
            "repo_memories",
            1,
            "owner namespace operations: the parent subquery of a predicate whose outer table is filtered by namespace",
        ),
        (
            "namespace_owner.rs",
            "repo_runs",
            1,
            "owner namespace operations: the parent subquery of a predicate whose outer table is filtered by namespace",
        ),
        ("oauth.rs", "am_model_cache", 1, WHOLE_PROJECT),
        ("operations.rs", "am_jobs", 1, WHOLE_PROJECT),
        ("product.rs", "am_call_reservations", 1, OWNER_ONLY),
        ("product.rs", "am_jobs", 2, OWNER_ONLY),
        ("product.rs", "am_model_calls", 1, OWNER_ONLY),
        ("search.rs", "am_call_reservations", 1, JOB_QUEUE),
        ("store.rs", "am_jobs", 1, OWNER_ONLY),
        ("store.rs", "am_query_cache", 1, WHOLE_PROJECT),
        (
            "temporal_graph.rs",
            "gf_alias_proposals",
            1,
            "accepted graph alias proposals of the code graph; Core writes none, so no namespaced row exists",
        ),
        ("vault.rs", "am_model_cache", 1, OWNER_ONLY),
        ("vector.rs", "am_vectors", 1, OWNER_ONLY),
        (
            "work_state.rs",
            "am_work_state",
            1,
            "the table's CREATE statement",
        ),
        (
            "workflow_retrieval.rs",
            "am_call_reservations",
            3,
            JOB_QUEUE,
        ),
        ("workflow_retrieval.rs", "am_model_calls", 2, JOB_QUEUE),
        ("workflows.rs", "am_call_reservations", 1, JOB_QUEUE),
        (
            "workflows.rs",
            "repo_memories",
            2,
            "inherit_visibility: the drafts this job's scoped claims::propose just wrote",
        ),
        (
            "workflows.rs",
            "repo_memory_revisions",
            1,
            "inherit_visibility: the drafts this job's scoped claims::propose just wrote",
        ),
    ];
    let tables: Vec<&str> = graphicle_runtime::memory_scope::TABLES
        .iter()
        .map(|(t, _)| *t)
        .collect();
    let found = unscoped_statements(&tables);
    let mut counts: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for (file, table, excerpt) in found {
        counts.entry((file, table)).or_default().push(excerpt);
    }
    let mut problems = Vec::new();
    for ((file, table), excerpts) in &counts {
        match ALLOW.iter().find(|(f, t, _, _)| f == file && t == table) {
            Some((_, _, allowed, _)) if *allowed == excerpts.len() => {}
            Some((_, _, allowed, _)) => problems.push(format!(
                "{file}: {} unscoped statements on {table}, {allowed} allowed (update the count and its reason only after review):\n  {}",
                excerpts.len(),
                excerpts.join("\n  ")
            )),
            None => problems.push(format!(
                "{file}: SQL on {table} without `memory_namespace`:\n  {}",
                excerpts.join("\n  ")
            )),
        }
    }
    for (file, table, _, _) in ALLOW {
        if !counts.contains_key(&(file.to_string(), table.to_string())) {
            problems.push(format!(
                "{file}: allowlist entry for {table} matches nothing; remove it"
            ));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// (file, table, excerpt) for every string literal in `src/` that reads like
/// SQL, names one of `tables` as a word, and does not mention
/// `memory_namespace`.
fn unscoped_statements(tables: &[&str]) -> Vec<(String, String, String)> {
    let verb = regex::Regex::new(r"\b(SELECT|INSERT|UPDATE|DELETE)\b").unwrap();
    let words: Vec<(&str, regex::Regex)> = tables
        .iter()
        .map(|table| (*table, regex::Regex::new(&format!(r"\b{table}\b")).unwrap()))
        .collect();
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files: Vec<_> = std::fs::read_dir(&src)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "rs"))
        .collect();
    files.sort();
    let mut out = Vec::new();
    for path in files {
        let text = std::fs::read_to_string(&path).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        for (line, body) in string_literals(&text) {
            if !verb.is_match(&body) || body.contains("memory_namespace") {
                continue;
            }
            for (table, word) in &words {
                if word.is_match(&body) {
                    let excerpt: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
                    out.push((
                        name.clone(),
                        table.to_string(),
                        format!(
                            "{name}:{line}: {}",
                            excerpt.chars().take(140).collect::<String>()
                        ),
                    ));
                }
            }
        }
    }
    out
}

/// The string literals of a Rust source file with their first line. A small
/// lexer, not a pattern: comments, char and byte literals (`'"'`) and
/// lifetimes are skipped, and a raw string ends only at a quote followed by
/// its own number of `#`, so no quote in code can pair two literals and hide
/// the SQL between them.
fn string_literals(text: &str) -> Vec<(usize, String)> {
    let b = text.as_bytes();
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let line = |at: usize| text[..at].matches('\n').count() + 1;
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let mut depth = 0;
                while i < b.len() {
                    if b[i..].starts_with(b"/*") {
                        depth += 1;
                        i += 2;
                    } else if b[i..].starts_with(b"*/") {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
            }
            b'r' if (i == 0
                || !ident(b[i - 1])
                || (b[i - 1] == b'b' && (i < 2 || !ident(b[i - 2]))))
                && {
                    let hashes = b[i + 1..].iter().take_while(|c| **c == b'#').count();
                    b.get(i + 1 + hashes) == Some(&b'"')
                } =>
            {
                let hashes = b[i + 1..].iter().take_while(|c| **c == b'#').count();
                let start = i + 2 + hashes;
                let close: Vec<u8> = std::iter::once(b'"')
                    .chain(std::iter::repeat_n(b'#', hashes))
                    .collect();
                let end = (start..b.len())
                    .find(|j| b[*j..].starts_with(&close))
                    .expect("unterminated raw string");
                out.push((line(i), text[start..end].to_string()));
                i = end + close.len();
            }
            b'"' => {
                let start = i + 1;
                let mut j = start;
                while b[j] != b'"' {
                    j += if b[j] == b'\\' { 2 } else { 1 };
                }
                out.push((line(i), text[start..j].to_string()));
                i = j + 1;
            }
            b'\'' => {
                // A char literal ('x', '\'', '\u{..}', a multi-byte char) or a
                // lifetime ('a), which has no closing quote.
                if b.get(i + 1) == Some(&b'\\') {
                    let mut j = i + 3;
                    while b[j] != b'\'' {
                        j += 1;
                    }
                    i = j + 1;
                } else {
                    let width = text[i + 1..].chars().next().map_or(1, char::len_utf8);
                    i += if b.get(i + 1 + width) == Some(&b'\'') {
                        width + 2
                    } else {
                        1
                    };
                }
            }
            _ => i += 1,
        }
    }
    out
}

/// The lexer the SQL check stands on: quotes inside char literals, comments
/// and raw strings never pair two literals.
#[test]
fn the_sql_check_reads_every_literal() {
    let source = r##"
        let quote = |c: char| c != '"' && c != '\\' && c != '\'';
        // a comment's "quote
        /* a block's " quote /* nested " */ */
        fn f<'a>(x: &'a str) {}
        let a = "SELECT 1 FROM am_vectors";
        let b = r#"SELECT "x" FROM am_jobs"#;
        let c = b'"';
        let d = "esc \" still";
        let e = br"raw bytes";
    "##;
    let found: Vec<String> = string_literals(source)
        .into_iter()
        .map(|(_, s)| s)
        .collect();
    assert_eq!(
        found,
        [
            "SELECT 1 FROM am_vectors",
            "SELECT \"x\" FROM am_jobs",
            "esc \\\" still",
            "raw bytes",
        ]
    );
}

/// B's secret in the parity tests: its memory says deploys use another
/// pipeline, so its passage ranks for the same question if it ever leaks.
const ORCA: &str = "ORCA4410";
const QUESTION: &str = "Which pipeline do deploys use?";

/// A model provider and a Qdrant collection for the parity tests. Vectors
/// count the words that tell the memories apart, so B's memory is as close
/// to the question as A's.
mod providers {
    use axum::{
        Json, Router,
        extract::{Path, State},
        http::StatusCode,
        routing::{get, post, put},
    };
    use serde_json::{Value, json};
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    pub struct Calls {
        pub embedded: Arc<Mutex<Vec<String>>>,
        pub observed: Arc<Mutex<usize>>,
        pub points: Arc<Mutex<BTreeMap<String, Value>>>,
        pub created: Arc<Mutex<Option<Value>>>,
    }

    pub fn vector(text: &str) -> Vec<f64> {
        let has = |word: &str| f64::from(u8::from(text.to_lowercase().contains(word)));
        vec![
            has("pipeline") + 0.01,
            has("approval") + 0.01,
            has("deploy") + 0.01,
        ]
    }

    async fn embeddings(State(calls): State<Calls>, Json(body): Json<Value>) -> Json<Value> {
        let inputs: Vec<String> = serde_json::from_value(body["input"].clone()).unwrap();
        calls
            .embedded
            .lock()
            .unwrap()
            .extend(inputs.iter().cloned());
        Json(
            json!({"data":inputs.iter().enumerate().map(|(i, text)| json!({"index":i,"embedding":vector(text)})).collect::<Vec<_>>(),
            "usage":{"prompt_tokens":25}}),
        )
    }

    /// Observer notes: one note quoting the rollback rule.
    async fn responses(State(calls): State<Calls>, Json(_): Json<Value>) -> Json<Value> {
        *calls.observed.lock().unwrap() += 1;
        let notes = json!({"notes":[{"text":"Rollbacks require two approvals.","quotes":["Rollbacks need two approvals."]}]});
        Json(
            json!({"status":"completed","model":"fixture-model","usage":{"input_tokens":150,"output_tokens":80},
            "output":[{"type":"message","content":[{"type":"output_text","text":notes.to_string()}]}]}),
        )
    }

    fn matches(point: &Value, condition: &Value) -> bool {
        if let Some(ids) = condition["has_id"].as_array() {
            return ids.contains(&point["id"]);
        }
        point["payload"][condition["key"].as_str().unwrap()] == condition["match"]["value"]
    }

    fn admitted(point: &Value, filter: &Value) -> bool {
        filter["must"]
            .as_array()
            .into_iter()
            .flatten()
            .all(|c| matches(point, c))
            && !filter["must_not"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|c| matches(point, c))
    }

    async fn collection(
        State(calls): State<Calls>,
        Path(_): Path<String>,
    ) -> (StatusCode, Json<Value>) {
        match calls.created.lock().unwrap().clone() {
            Some(created) => (
                StatusCode::OK,
                Json(json!({"result":{"config":{"params":{"vectors":created}}}})),
            ),
            None => (StatusCode::NOT_FOUND, Json(json!({}))),
        }
    }
    async fn create(
        State(calls): State<Calls>,
        Path(_): Path<String>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        *calls.created.lock().unwrap() = Some(body["vectors"].clone());
        Json(json!({"result":true}))
    }
    async fn upsert(
        State(calls): State<Calls>,
        Path(_): Path<String>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        let mut points = calls.points.lock().unwrap();
        for point in body["points"].as_array().unwrap() {
            points.insert(point["id"].as_str().unwrap().to_string(), point.clone());
        }
        Json(json!({"result":{"status":"completed"}}))
    }
    async fn delete(
        State(calls): State<Calls>,
        Path(_): Path<String>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        calls
            .points
            .lock()
            .unwrap()
            .retain(|_, point| !admitted(point, &body["filter"]));
        Json(json!({"result":{"status":"completed"}}))
    }
    async fn query(
        State(calls): State<Calls>,
        Path(_): Path<String>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        let question: Vec<f64> = serde_json::from_value(body["query"].clone()).unwrap();
        let dot = |v: &Value| -> f64 {
            v.as_array()
                .unwrap()
                .iter()
                .zip(&question)
                .map(|(a, b)| a.as_f64().unwrap() * b)
                .sum()
        };
        let mut found: Vec<Value> = calls
            .points
            .lock()
            .unwrap()
            .values()
            .filter(|point| admitted(point, &body["filter"]))
            .map(|point| json!({"id":point["id"],"score":dot(&point["vector"]),"payload":point["payload"]}))
            .collect();
        found.sort_by(|a, b| {
            b["score"]
                .as_f64()
                .partial_cmp(&a["score"].as_f64())
                .unwrap()
        });
        Json(json!({"result":{"points":found}}))
    }

    pub struct Server {
        pub address: std::net::SocketAddr,
        pub calls: Calls,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    pub async fn start() -> Server {
        let calls = Calls::default();
        let app = Router::new()
            .route("/v1/embeddings", post(embeddings))
            .route("/v1/responses", post(responses))
            .route("/qdrant/collections/{c}", get(collection).put(create))
            .route("/qdrant/collections/{c}/points", put(upsert))
            .route("/qdrant/collections/{c}/points/delete", post(delete))
            .route("/qdrant/collections/{c}/points/query", post(query))
            .with_state(calls.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Server {
            address,
            calls,
            task,
        }
    }
}

impl Fixture {
    fn models(&self, server: &providers::Server) -> model::Config {
        serde_json::from_value(
            json!({"default_model":"fixture","embedding_model":"fixture","models":{"fixture":{
            "protocol":"responses","base_url":format!("http://{}/v1",server.address),
            "model":"fixture-model","input_limit":16000,"output_limit":2048}}}),
        )
        .unwrap()
    }
    /// Queue a job as `key`, run it and return its status.
    async fn run(&self, key: &Key, config: &model::Config, args: Value) -> Value {
        let job = self
            .store
            .enqueue(&self.workspace, &key.capability, &self.args(args), config)
            .unwrap();
        let worker = jobs::Worker::new(self.store.clone(), config.clone()).unwrap();
        assert!(worker.once().await.unwrap());
        self.store
            .job_status(
                &self.workspace,
                &key.capability,
                &self.args(json!({"job_id":job["id"]})),
            )
            .unwrap()
    }
    /// A reviewed memory saying `body`, as `key` (approved by `reviewer`, or
    /// by `key` itself in a self-reviewing namespace). Returns its id.
    fn remember(&self, key: &Key, reviewer: &Key, tag: &str, body: &str) -> String {
        let proposed = self
            .command(
                key,
                "memory.propose",
                json!({"kind":"decision","title":"Deploy rule","body":body,"idempotency_key":format!("{tag}-rule"),
                    "evidence":[{"source_id":"file:notes.txt","quote":"Deploys go through the blue pipeline."}]}),
            )
            .unwrap();
        let memory = s(&proposed, "memory_id").to_string();
        self.command(
            reviewer,
            "learning.review",
            json!({"memory_id":memory,"expected_revision":1,"decision":"approve","maintainer":"r","reason":"checked"}),
        )
        .unwrap();
        memory
    }
    fn vector_rows(&self) -> Vec<Value> {
        self.store
            .authorized(&self.workspace, &self.owner, "read", |db, _| {
                graphicle_runtime::store::rows(
                    db,
                    "SELECT identity,memory_namespace FROM am_vectors WHERE repository_id=? ORDER BY identity",
                    [&self.repo],
                )
            })
            .unwrap()
    }
}

/// Ids, digests and clocks differ between two stores (and so the token
/// counts of texts that quote ids); everything else a reply says must not.
fn masked(reply: &Value) -> Value {
    // Text that quotes JSON (an MCP reply's content) is compared as JSON;
    // the process's write-lock counters, which every store and test of this
    // process adds to, are not compared.
    fn unfold(value: &mut Value) {
        match value {
            Value::Object(fields) => {
                fields.remove("write_lock");
                fields.values_mut().for_each(unfold);
            }
            Value::Array(items) => items.iter_mut().for_each(unfold),
            Value::String(text) if text.starts_with('{') => {
                if let Ok(mut inner) = serde_json::from_str::<Value>(text) {
                    unfold(&mut inner);
                    *value = inner;
                }
            }
            _ => {}
        }
    }
    let mut value = reply.clone();
    unfold(&mut value);
    let rules = [
        (r"[0-9a-f]{64}", "<sha>"),
        (r"[0-9a-f]{32}", "<id>"),
        (
            r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?(Z|[+-]\d\d:\d\d)",
            "<time>",
        ),
        (
            r#""(elapsed_ms|ms|duration_ms|[a-z_]*seconds)":[0-9.e+-]+"#,
            r#""$1":0"#,
        ),
        // Random hex ids tokenize to different lengths; the text is compared.
        (r#""tokens":\d+"#, r#""tokens":0"#),
    ];
    let mut text = value.to_string();
    for (pattern, with) in rules {
        text = regex::Regex::new(pattern)
            .unwrap()
            .replace_all(&text, with)
            .into_owned();
    }
    serde_json::from_str(&text).unwrap()
}

/// Where two replies first differ: the JSON path and both values.
fn difference(path: &str, left: &Value, right: &Value) -> String {
    match (left, right) {
        (Value::Object(l), Value::Object(r)) => {
            for key in l.keys().chain(r.keys()) {
                let (a, b) = (
                    l.get(key).unwrap_or(&Value::Null),
                    r.get(key).unwrap_or(&Value::Null),
                );
                if a != b || l.contains_key(key) != r.contains_key(key) {
                    return difference(&format!("{path}.{key}"), a, b);
                }
            }
            String::new()
        }
        (Value::Array(l), Value::Array(r)) if l.len() == r.len() => l
            .iter()
            .zip(r)
            .enumerate()
            .find(|(_, (a, b))| a != b)
            .map(|(i, (a, b))| difference(&format!("{path}[{i}]"), a, b))
            .unwrap_or_default(),
        _ => format!("at {path}: shared {left:#} | namespaced {right:#}"),
    }
}

/// What an agent sees after one history: it writes a reviewed memory,
/// embeds the project, observes the notes and asks with dense retrieval,
/// cached query vectors, observer notes and Qdrant. `between` runs after
/// the writes and before the reads (another namespace's work).
async fn history<'a>(
    f: &'a Fixture,
    server: &'a providers::Server,
    agent: &'a Key,
    reviewer: &'a Key,
    between: impl AsyncFnOnce(),
) -> Vec<(&'static str, Value)> {
    let config = f.models(server);
    let memory = f.remember(agent, reviewer, "rule", "Deploys use the blue pipeline.");
    let mut seen = Vec::new();
    seen.push((
        "work_state.write",
        f.command(
            agent,
            "work_state.write",
            json!({"list":"Release","fields":{"task":"ship it","status":"doing"}}),
        )
        .unwrap(),
    ));
    let prepared = f
        .command(
            agent,
            "interpretation.prepare",
            json!({"source_id":"file:notes.txt"}),
        )
        .unwrap();
    let statement = "Rollbacks need two approvals.";
    let claim = json!({"statement":statement,"subject":"rollbacks","relation":"need","object":"two approvals",
        "speaker":"document","modality":"requirement","conditions":[],"action":"ADD","target_id":null,
        "valid_at":null,"invalid_at":null,"time_basis":"unknown","anchor_id":null,
        "evidence":[{"id":"file:notes.txt","quote":statement}]});
    let proposed = f
        .command(agent, "interpretation.propose", json!({"source_id":"file:notes.txt","context_sha256":prepared["sha256"],"claims":[claim]}))
        .unwrap();
    seen.push(("interpretation.propose", proposed.clone()));
    seen.push((
        "learning.review",
        f.command(reviewer, "learning.review", json!({"memory_id":proposed["drafts"][0]["memory_id"],"expected_revision":1,"decision":"approve","maintainer":"r","reason":"checked"}))
            .unwrap(),
    ));
    seen.push((
        "embed",
        f.run(agent, &config, json!({"kind":"embed","max_calls":2}))
            .await,
    ));
    seen.push((
        "observer_notes",
        f.run(
            agent,
            &config,
            json!({"kind":"observer_notes","include":["file:notes.txt"],"max_calls":2}),
        )
        .await,
    ));
    between().await;
    let recall = json!({"kind":"recall","question":QUESTION,"max_calls":2});
    seen.push(("recall", f.run(agent, &config, recall.clone()).await));
    // Another job (a replay of the same input would return the first).
    let mut again = recall.clone();
    again["max_calls"] = json!(3);
    seen.push((
        "recall again (cached query vector)",
        f.run(agent, &config, again).await,
    ));
    seen.push((
        "recall with note keys",
        f.run(agent, &config, json!({"kind":"recall","question":"How many approvals do rollbacks need?","note_keys":true,"max_calls":2})).await,
    ));
    // The owner publishes the project's vectors; the agent ranks with them.
    let worker = jobs::Worker::new(f.store.clone(), config.clone()).unwrap();
    graphicle_runtime::vector::configure(
        &f.store,
        &f.workspace,
        &f.owner,
        &f.args(
            json!({"endpoint":format!("http://{}/qdrant",server.address),"collection":"parity"}),
        ),
    )
    .unwrap();
    let key = graphicle_runtime::model::key(config.profile(Some("fixture")).unwrap().1);
    graphicle_runtime::vector::publish(
        &f.store,
        &worker.client,
        &f.workspace,
        &f.owner,
        &f.args(json!({"model_key":key})),
    )
    .await
    .unwrap();
    seen.push((
        "recall over Qdrant",
        f.run(
            agent,
            &config,
            json!({"kind":"recall","question":QUESTION,"vector_backend":"qdrant","max_calls":2}),
        )
        .await,
    ));
    for (label, operation, args) in [
        ("context", "context", json!({"question":QUESTION})),
        ("memories", "memories", json!({"include_retired":true})),
        (
            "memory.search",
            "memory.search",
            json!({"question":QUESTION}),
        ),
        ("memory.export", "memory.export", json!({})),
        ("status", "status", json!({})),
        ("workflow.status", "workflow.status", json!({})),
        ("pack", "pack", json!({"question":QUESTION})),
        (
            "observer.context",
            "observer.context",
            json!({"question":"rollbacks approvals"}),
        ),
        (
            "work_state.read",
            "work_state.read",
            json!({"include_closed":true}),
        ),
        (
            "memory.history",
            "memory.history",
            json!({"memory_id":memory}),
        ),
        ("claim.history", "claim.history", json!({})),
        ("memory.template", "memory.template", json!({})),
    ] {
        seen.push((label, f.command(agent, operation, args).unwrap()));
    }
    for (name, arguments) in [
        (
            "recall_project",
            json!({"question":QUESTION,"structured":true}),
        ),
        ("project_memory", json!({"include_retired":true})),
        ("search_project_memory", json!({"question":QUESTION})),
        ("project_memory_history", json!({"memory_id":memory})),
        ("repository_status", json!({})),
        ("workflow_status", json!({})),
        ("work_state_read", json!({"include_closed":true})),
    ] {
        seen.push((name, f.tool(agent, name, arguments)));
    }
    seen.push((
        "initialize",
        api::mcp(&f.app(), &f.workspace, &agent.capability, &f.repo,
            &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"probe","version":"1"}}}))
        .unwrap(),
    ));
    seen.push((
        "tools/list",
        api::mcp(
            &f.app(),
            &f.workspace,
            &agent.capability,
            &f.repo,
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}),
        )
        .unwrap(),
    ));
    seen
}

/// Parity: an agent of namespace A on a project it shares with namespace B
/// sees exactly what a shared agent sees on a project of its own, through
/// dense vectors, cached query vectors, observer notes and Qdrant, with only
/// ids, digests and clocks differing. And B's work in between leaves no
/// trace in what A sees.
#[tokio::test]
async fn a_namespaced_key_sees_what_a_shared_key_sees() {
    let server = providers::start().await;
    let alone = Fixture::new();
    let agent = alone.key("agent", None, None);
    let reviewer = alone.key("reviewer", None, None);
    let shared = history(&alone, &server, &agent, &reviewer, async || {}).await;

    let server = providers::start().await;
    let together = Fixture::new();
    let a = together.key("agent", Some("dev-a"), Some(true));
    let b = together.key("agent", Some("dev-b"), Some(true));
    let config = together.models(&server);
    let b_memory = std::cell::RefCell::new(String::new());
    let private = history(&together, &server, &a, &a, async || {
        *b_memory.borrow_mut() = together.remember(&b, &b, "orca", &format!("{ORCA}: deploys use the red pipeline."));
        let embedded = together.run(&b, &config, json!({"kind":"embed","max_calls":2})).await;
        assert_eq!(embedded["result"]["embedded_passages"], 1, "B embeds only its memory: {embedded:#}");
        let recalled = together.run(&b, &config, json!({"kind":"recall","question":QUESTION,"max_calls":2})).await;
        assert!(recalled.to_string().contains(ORCA), "B recalls its own memory densely: {recalled:#}");
        assert_eq!(recalled["result"]["semantic"]["loaded"], true);
        // B's cache is full; A's questions must not evict any of it.
        together
            .store
            .transaction(|db| {
                for i in 0..600 {
                    db.execute(
                        "INSERT INTO am_query_cache(repository_id,request_hash,result,memory_namespace) VALUES(?,?,?,?)",
                        graphicle_runtime::params![together.repo, format!("b-question-{i}"), "[1.0,0.0,0.0]", "dev-b"],
                    )?;
                }
                Ok(())
            })
            .unwrap();
    })
    .await;
    let b_memory = b_memory.into_inner();

    for ((label, theirs), (_, ours)) in shared.iter().zip(&private) {
        assert!(
            !ours.to_string().contains(ORCA),
            "{label}: A sees B's memory: {ours:#}"
        );
        assert!(
            !ours.to_string().contains(&b_memory),
            "{label}: A sees B's id: {ours:#}"
        );
        assert!(
            !ours.to_string().contains("memory_namespace")
                && !theirs.to_string().contains("memory_namespace"),
            "{label}: a reply names a namespace: {ours:#}"
        );
        let (theirs, ours) = (masked(theirs), masked(ours));
        assert!(
            theirs == ours,
            "{label} differs: {}",
            difference("", &theirs, &ours)
        );
    }
    // The features were really in use, for both.
    for seen in [&shared, &private] {
        let get = |label: &str| &seen.iter().find(|(l, _)| *l == label).unwrap().1;
        for label in ["recall", "recall over Qdrant"] {
            assert_eq!(get(label)["result"]["semantic"]["loaded"], true, "{label}");
            // The dense arm ranked something, not just loaded.
            assert!(
                get(label)["result"]["sources"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|e| s(e, "reason").contains("semantic")),
                "{label}: {:#}",
                get(label)["result"]
            );
            assert!(
                s(&get(label)["result"], "markdown").contains("Deploys use the blue pipeline.")
            );
        }
        assert_eq!(
            get("recall again (cached query vector)")["calls"][0]["cached"],
            1
        );
        assert_eq!(get("observer_notes")["result"]["notes"]["kept_now"], 1);
        assert_eq!(get("observer.context")["selected"], 1);
    }
    // Where each row went: sources and shared memory shared, each
    // namespace's memory under its namespace, in vectors and query vectors.
    let rows = together.vector_rows();
    let namespaces: Vec<Option<&str>> = rows
        .iter()
        .map(|r| r["memory_namespace"].as_str())
        .collect();
    assert_eq!(
        rows.iter()
            .filter(|r| !s(r, "identity").starts_with("memory:"))
            .count(),
        2
    );
    assert!(
        rows.iter()
            .filter(|r| !s(r, "identity").starts_with("memory:"))
            .all(|r| r["memory_namespace"].is_null())
    );
    assert!(
        namespaces.contains(&Some("dev-a")) && namespaces.contains(&Some("dev-b")),
        "{rows:#?}"
    );
    assert_eq!(
        rows.iter()
            .find(|r| s(r, "identity") == format!("memory:{b_memory}"))
            .unwrap()["memory_namespace"],
        "dev-b"
    );
    let cached = together
        .store
        .authorized(&together.workspace, &together.owner, "read", |db, _| {
            graphicle_runtime::store::rows(db, "SELECT DISTINCT memory_namespace FROM am_query_cache WHERE repository_id=? ORDER BY memory_namespace", [&together.repo])
        })
        .unwrap();
    assert_eq!(
        json!(cached),
        json!([{"memory_namespace":"dev-a"},{"memory_namespace":"dev-b"}])
    );
    let kept = together
        .store
        .authorized(&together.workspace, &together.owner, "read", |db, _| {
            graphicle_runtime::store::rows(db, "SELECT count(*) AS n FROM am_query_cache WHERE repository_id=? AND memory_namespace='dev-b'", [&together.repo])
        })
        .unwrap();
    assert_eq!(
        kept[0]["n"], 601,
        "A's questions evicted B's cached query vectors"
    );
    // Qdrant holds every scope's points, each in its part.
    let parts: std::collections::BTreeSet<String> = server
        .calls
        .points
        .lock()
        .unwrap()
        .values()
        .map(|p| s(&p["payload"], "partition").to_string())
        .collect();
    assert_eq!(
        parts.into_iter().collect::<Vec<_>>(),
        ["memory:dev-a", "memory:dev-b", "source"]
    );
}

/// A store with a namespace needs a program that keeps namespaces apart:
/// an earlier release would read a namespaced key as an ordinary key and its
/// memory as shared, so it must refuse the store. A store that never had a
/// namespace asks for nothing new.
#[test]
fn a_store_with_a_namespace_refuses_programs_that_cannot_keep_it() {
    let f = Fixture::new();
    let readers = |f: &Fixture| {
        f.store
            .authorized(&f.workspace, &f.owner, "read", |db, _| {
                graphicle_runtime::store::rows(
                    db,
                    "SELECT reason,format,version FROM store_readers ORDER BY reason",
                    [],
                )
            })
            .unwrap()
    };
    f.key("agent", None, None);
    assert!(
        readers(&f).iter().all(|r| r["format"].as_i64() < Some(2)),
        "{:#?}",
        readers(&f)
    );
    f.key("agent", Some("dev-a"), Some(true));
    assert!(
        readers(&f)
            .iter()
            .any(|r| r["reason"] == "memory_namespaces"
                && r["format"] == 2
                && r["version"] == "0.1.19"),
        "{:#?}",
        readers(&f)
    );
    // A store whose record went missing (copied by hand, or made by a
    // build that did not record it) gets it back when it is next opened.
    f.store
        .transaction(|db| {
            db.execute(
                "DELETE FROM store_readers WHERE reason='memory_namespaces'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    let reopened = Store::open(f._dir.path()).unwrap();
    let rows = reopened
        .authorized(&f.workspace, &f.owner, "read", |db, _| {
            graphicle_runtime::store::rows(
                db,
                "SELECT format FROM store_readers WHERE reason='memory_namespaces'",
                [],
            )
        })
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["format"], 2);
}
