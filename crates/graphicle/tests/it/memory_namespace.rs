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
    // A shared key's MCP tools say what they always said; a namespaced
    // key's memory tools say its memory is private.
    let described = |key: &Key| -> String {
        let listed = api::mcp(
            &f.app(),
            &f.workspace,
            &key.capability,
            &f.repo,
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}),
        )
        .unwrap();
        listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "recall_project")
            .map(|t| s(t, "description").to_string())
            .unwrap()
    };
    assert!(!described(&shared_agent).contains(api::PRIVATE_MEMORY));
    assert!(described(&a).ends_with(api::PRIVATE_MEMORY));
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
    const SHARED_ONLY: &str = "reached only for a shared (NULL) principal: namespaced callers are refused or skipped before it (dense vectors, query embeddings and observer notes stay out of namespaces)";
    const SCOPED_PARENT: &str =
        "updates a row by the id a namespace-scoped query of the same transaction just returned";
    const ALLOW: &[(&str, &str, usize, &str)] = &[
        ("auto_observe.rs", "am_jobs", 2, OWNER_ONLY),
        ("auto_observe.rs", "am_observer_notes", 1, SHARED_ONLY),
        ("claims.rs", "am_claims", 3, SCOPED_PARENT),
        ("connectors.rs", "am_jobs", 3, OWNER_ONLY),
        ("connectors.rs", "am_model_cache", 2, WHOLE_PROJECT),
        (
            "deletion.rs",
            "repo_memories",
            1,
            "certified forgetting of a source: the post-delete residue check over every namespace",
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
        ("observer.rs", "am_observer_notes", 5, SHARED_ONLY),
        ("observer.rs", "am_observer_reflections", 3, SHARED_ONLY),
        ("operations.rs", "am_jobs", 1, WHOLE_PROJECT),
        ("product.rs", "am_call_reservations", 1, OWNER_ONLY),
        ("product.rs", "am_jobs", 2, OWNER_ONLY),
        ("product.rs", "am_model_calls", 1, OWNER_ONLY),
        ("query.rs", "am_vectors", 1, SHARED_ONLY),
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
        ("workflows.rs", "am_query_cache", 3, SHARED_ONLY),
        ("workflows.rs", "am_vectors", 3, SHARED_ONLY),
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
    let literal = regex::Regex::new(r#"(?s)r(#*)"(.*?)"|"((?:[^"\\]|\\.)*)""#).unwrap();
    let verb = regex::Regex::new(r"\b(SELECT|INSERT|UPDATE|DELETE)\b").unwrap();
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
        for m in literal.captures_iter(&text) {
            let body = m.get(2).or_else(|| m.get(3)).map_or("", |b| b.as_str());
            if !verb.is_match(body) || body.contains("memory_namespace") {
                continue;
            }
            for table in tables {
                let word = regex::Regex::new(&format!(r"\b{table}\b")).unwrap();
                if word.is_match(body) {
                    let line = text[..m.get(0).unwrap().start()].matches('\n').count() + 1;
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
