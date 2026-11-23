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

/// Strip process-global `write_lock` diagnostics before checking for the word
/// `"memory_namespace"`, because `Store::transaction` records caller file
/// paths (`tests/it/memory_namespace.rs:...`) there.
fn without_write_lock(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                if k == "write_lock" {
                    continue;
                }
                out.insert(k.clone(), without_write_lock(v));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(without_write_lock).collect()),
        Value::String(text) if text.contains("write_lock") => {
            if let Ok(parsed) = serde_json::from_str::<Value>(text) {
                Value::String(without_write_lock(&parsed).to_string())
            } else {
                Value::String(text.clone())
            }
        }
        other => other.clone(),
    }
}

fn has_json_key(value: &Value, key: &str) -> bool {
    match value {
        Value::Object(map) => map.contains_key(key) || map.values().any(|v| has_json_key(v, key)),
        Value::Array(items) => items.iter().any(|v| has_json_key(v, key)),
        _ => false,
    }
}

fn assert_clean(label: &str, reply: &Value, forbidden: &[String]) {
    assert!(
        !has_json_key(reply, "memory_namespace"),
        "{label}: reply carries a memory_namespace field: {reply:#}"
    );
    let text = reply.to_string();
    let without_lock = without_write_lock(reply).to_string();
    for trace in forbidden {
        let haystack = if trace == "memory_namespace" {
            &without_lock
        } else {
            &text
        };
        assert!(
            !haystack.contains(trace.as_str()),
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
    // The namespace's keys go with it.
    assert_eq!(deleted["credentials_revoked"], 1, "{deleted:#}");
    assert!(f.command(&a, "memories", json!({})).is_err());
    // A new key for the namespace, which only the owner makes, finds it empty.
    let a = f.key("agent", Some("dev-a"), None);
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
    // A vector row that is not one of the archive's memories' passages (a
    // shared source passage, say) is refused, even with a matching SHA-256.
    for identity in ["file:notes.txt", "memory:not-in-the-archive"] {
        let mut forged = exported.clone();
        forged["archive"]["tables"]["am_vectors"] = json!([{"repository_id":f.repo,"identity":identity,
            "source_sha":"x","model_key":"m","dimensions":1,"vector":"[0.5]","memory_namespace":"dev-a"}]);
        forged["sha256"] = json!(graphicle_runtime::digest(&forged["archive"]));
        let refused = f
            .store
            .namespace_import(&f.workspace, &f.owner, "dev-a", &forged)
            .unwrap_err();
        assert!(
            refused.message.contains("vector of no memory"),
            "{identity}: {}",
            refused.message
        );
    }
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
        ("api.rs", "credentials", 1, OWNER_ONLY),
        (
            "archive.rs",
            "<dynamic>",
            4,
            "whole-store backup/restore copy and PostgreSQL identity-sequence reset (owner/CLI only)",
        ),
        ("auto_observe.rs", "am_jobs", 2, OWNER_ONLY),
        ("claims.rs", "am_claims", 3, SCOPED_PARENT),
        ("connectors.rs", "am_jobs", 3, OWNER_ONLY),
        ("connectors.rs", "am_model_cache", 2, WHOLE_PROJECT),
        ("connectors.rs", "credentials", 1, OWNER_ONLY),
        (
            "db.rs",
            "<dynamic>",
            1,
            "SQLite autoincrement sequence helper (restore/migration internals)",
        ),
        (
            "deletion.rs",
            "<dynamic>",
            19,
            "certified deletion, residue scans and receipt verification: memory.delete binds memory_namespace via instr_query; connector/project/namespace deletion is owner/source-wide",
        ),
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
            8,
            "memory.delete by scoped ids (2), certified forgetting of a source (3), and the owner's deletion.state digest and deletion.verify (3, one of them a deleted namespace's receipt, over every project of the workspace)",
        ),
        (
            "deletion.rs",
            "repo_memory_revisions",
            3,
            "memory.delete by scoped ids (2), and certified forgetting of a source (1)",
        ),
        ("doc_links.rs", "am_jobs", 4, JOB_QUEUE),
        (
            "graph_delta.rs",
            "<dynamic>",
            1,
            "code graph owner-column migration on shared gf_nodes/gf_edges (not namespaced)",
        ),
        (
            "graph_projection.rs",
            "<dynamic>",
            2,
            "code graph node/edge expiration on shared gf_nodes/gf_edges (not namespaced)",
        ),
        ("graph_projection.rs", "credentials", 1, OWNER_ONLY),
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
            "metering.rs",
            "credentials",
            3,
            "admin usage reports joining credential labels (2) and a unit-test helper (1)",
        ),
        (
            "namespace_owner.rs",
            "<dynamic>",
            3,
            "owner namespace export, restore and erase over TABLES (each gated by owner authorize() and memory_namespace=?)",
        ),
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
        ("operations.rs", "<dynamic>", 1, WHOLE_PROJECT),
        ("operations.rs", "am_jobs", 1, WHOLE_PROJECT),
        ("operations.rs", "am_model_calls", 1, WHOLE_PROJECT),
        (
            "passage_index.rs",
            "<dynamic>",
            2,
            "shared source passage-index table cleanup (not namespaced)",
        ),
        ("product.rs", "am_call_reservations", 1, OWNER_ONLY),
        ("product.rs", "am_jobs", 2, OWNER_ONLY),
        ("product.rs", "am_model_calls", 1, OWNER_ONLY),
        ("search.rs", "am_call_reservations", 1, JOB_QUEUE),
        ("retention.rs", "<dynamic>", 1, OWNER_ONLY),
        (
            "retention.rs",
            "repo_memories",
            1,
            "source retention (admin only, preview and apply): the sources every namespace's reviewed memory cites, which it keeps",
        ),
        ("store.rs", "am_jobs", 1, OWNER_ONLY),
        ("store.rs", "am_query_cache", 1, WHOLE_PROJECT),
        (
            "store.rs",
            "credentials",
            4,
            "credential revocation (3, admin/owner only) and one JSON path `'$.credentials'` in connector ACL lookup (1)",
        ),
        (
            "temporal_graph.rs",
            "gf_alias_proposals",
            1,
            "a migration's backfill of accepted alias proposals: shared ones only (`memory_namespace IS NULL`, added to the statement once the column exists)",
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
    let mut tables: Vec<&str> = graphicle_runtime::memory_scope::TABLES
        .iter()
        .map(|(t, _)| *t)
        .collect();
    tables.push("credentials");
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
    let dynamic_table = regex::Regex::new(r#"\b(?:FROM|INTO|UPDATE|JOIN)\s+"?\{[^}]*\}"#).unwrap();
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
            let excerpt: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
            let formatted = format!(
                "{name}:{line}: {}",
                excerpt.chars().take(140).collect::<String>()
            );
            for (table, word) in &words {
                if word.is_match(&body) {
                    out.push((name.clone(), table.to_string(), formatted.clone()));
                }
            }
            if dynamic_table.is_match(&body) {
                out.push((name.clone(), "<dynamic>".to_string(), formatted));
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
            !has_json_key(ours, "memory_namespace")
                && !has_json_key(theirs, "memory_namespace")
                && !without_write_lock(ours)
                    .to_string()
                    .contains("memory_namespace")
                && !without_write_lock(theirs)
                    .to_string()
                    .contains("memory_namespace"),
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

/// The store formats a store asks of the programs that open it.
fn readers(store: &Store, f: &Fixture) -> Vec<Value> {
    store
        .authorized_read(&f.workspace, &f.owner, "read", |db, _| {
            graphicle_runtime::store::rows(
                db,
                "SELECT reason,format,version FROM store_readers ORDER BY reason",
                [],
            )
        })
        .unwrap()
}

/// Whether `readers` asks for store format 2, first read by 0.1.20.
fn needs_namespace_reader(readers: &[Value]) -> bool {
    readers
        .iter()
        .any(|r| r["reason"] == "memory_namespaces" && r["format"] == 2 && r["version"] == "0.1.20")
}

/// A store with a namespace needs a program that keeps namespaces apart:
/// an earlier release would read a namespaced key as an ordinary key and its
/// memory as shared, so it must refuse the store. A store whose record went
/// missing gets it back when it is next opened.
#[test]
fn a_store_with_a_namespace_refuses_programs_that_cannot_keep_it() {
    let f = Fixture::new();
    f.key("agent", Some("dev-a"), Some(true));
    assert!(
        needs_namespace_reader(&readers(&f.store, &f)),
        "{:#?}",
        readers(&f.store, &f)
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
    assert!(
        needs_namespace_reader(&readers(&reopened, &f)),
        "{:#?}",
        readers(&reopened, &f)
    );
}

/// Downgrading a store that never had a namespace. Opening it adds the
/// `memory_namespace` columns, and releases before 0.1.20 insert into those
/// tables without naming their columns, which SQLite refuses once a table
/// has one more column. So the store records store format 2 as soon as it
/// has the columns, and an earlier release (store format 1 at most) refuses
/// to open it instead of failing at its first save.
#[test]
fn a_store_that_never_had_a_namespace_is_refused_by_earlier_releases() {
    let f = Fixture::new();
    f.key("agent", None, None);
    let sqlite = f.store.engine() == graphicle_runtime::db::Engine::Sqlite;
    let namespaced: Vec<&str> = std::iter::once("credentials")
        .chain(
            graphicle_runtime::memory_scope::TABLES
                .iter()
                .map(|(table, _)| *table),
        )
        .collect();
    if sqlite {
        // The store as 0.1.19 left it: no namespace column anywhere, no
        // record. (SQLite cannot drop an indexed column, so the index goes
        // first.) On a plain connection: the store's transactions report
        // where the slowest ran, and this file's name is a namespace trace
        // the other tests look for in replies.
        let mut sql = String::from(
            "BEGIN IMMEDIATE; DELETE FROM store_readers WHERE reason='memory_namespaces';",
        );
        for table in &namespaced {
            sql.push_str(&format!(
                " DROP INDEX IF EXISTS {table}_namespace_lookup; ALTER TABLE {table} DROP COLUMN memory_namespace;"
            ));
        }
        sql.push_str(" COMMIT;");
        f.store.connect().unwrap().execute_batch(&sql).unwrap();
        assert!(!needs_namespace_reader(&readers(&f.store, &f)));
    }
    let store = Store::open(f._dir.path()).unwrap();
    let recorded = readers(&store, &f);
    assert!(needs_namespace_reader(&recorded), "{recorded:#?}");
    // What an earlier release checks when it opens a store: the newest
    // format asked for, against the one it reads (1, from 0.1.3 to 0.1.19).
    let newest = recorded
        .iter()
        .filter_map(|r| r["format"].as_i64())
        .max()
        .unwrap();
    assert!(newest > 1, "{recorded:#?}");
    // Nothing in the store is namespaced.
    let namespaced_rows = store
        .authorized_read(&f.workspace, &f.owner, "read", |db, _| {
            let mut count = 0;
            for table in &namespaced {
                count += db.query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE memory_namespace IS NOT NULL"),
                    [],
                    |row| row.get::<_, i64>(0),
                )?;
            }
            Ok(count)
        })
        .unwrap();
    assert_eq!(namespaced_rows, 0);
    if sqlite {
        // Why the record is needed: 0.1.19's positional insert of a memory
        // (ten values) is refused now that the table has an eleventh column.
        let refused = store
            .transaction(|db| {
                db.execute(
                    "INSERT INTO repo_memories VALUES(?,?,?,?,?,?,?,?,?,?)",
                    graphicle_runtime::db::params![
                        "m-old", f.repo, "decision", 1, "draft", "t", "b", "{}", "now", "now"
                    ],
                )?;
                Ok(())
            })
            .unwrap_err()
            .to_string();
        assert!(refused.contains("values were supplied"), "{refused}");
    }
}

/// How many rows of `table` hold `needle` in any of `columns`.
fn holding(f: &Fixture, table: &str, columns: &[&str], needle: &str) -> i64 {
    let filter = columns
        .iter()
        .map(|c| format!("instr({c},?)>0"))
        .collect::<Vec<_>>()
        .join(" OR ");
    let values: Vec<&str> = columns.iter().map(|_| needle).collect();
    f.store
        .read(|db| {
            Ok(db.query_row(
                &format!("SELECT count(*) FROM {table} WHERE {filter}"),
                graphicle_runtime::db::params_from_iter(values),
                |row| row.get::<_, i64>(0),
            )?)
        })
        .unwrap()
}

/// What is left after a namespace delete: nothing of the namespace in any
/// table of the store (its own rows, and the served and outcome records,
/// hints and dependency-index entries that name its memories), its vectors
/// queued for removal from the project's vector backend, and none of its
/// keys valid. Another namespace keeps everything.
#[test]
fn deleting_a_namespace_leaves_nothing_of_it() {
    let f = Fixture::new();
    let a = f.key("agent", Some("dev-a"), Some(true));
    let reader = f.key("reader", Some("dev-a"), None);
    let b = f.key("agent", Some("dev-b"), Some(true));
    let ids = f.write_as(&a, "A");
    let b_ids = f.write_as(&b, "B");
    f.recall(&a, "deploy rule blue pipeline");
    let (memory, b_memory) = (ids["memory"].clone(), b_ids["memory"].clone());
    // What names a memory outside the namespace's own rows: served and
    // outcome records and hints (an extension writes them), and the stale
    // marks of the dependency index; and a vector backend for the project.
    // (On a plain connection, as the downgrade test explains.)
    let db = f.store.connect().unwrap();
    db.execute(
        "INSERT INTO am_backends VALUES(?1,'qdrant','{\"url\":\"http://127.0.0.1:9\",\"collection\":\"memory\"}')",
        [&f.repo],
    )
    .unwrap();
    for (tag, id) in [("a", &memory), ("b", &b_memory)] {
        let named = json!({"memories":[id]}).to_string();
        db.execute(
            "INSERT INTO gf_served(request_hash,workspace,repository_id,question,served,created_at) VALUES(?,?,?,'q',?,'now')",
            graphicle_runtime::db::params![format!("served-{tag}"), f.workspace, f.repo, named],
        )
        .unwrap();
        db.execute(
            "INSERT INTO gf_outcomes(id,workspace,repository_id,request_hash,outcome,targets,created_at) VALUES(?,?,?,?,'used',?,'now')",
            graphicle_runtime::db::params![format!("outcome-{tag}"), f.workspace, f.repo, format!("served-{tag}"), named],
        )
        .unwrap();
        db.execute(
            "INSERT INTO gf_hints(id,workspace,repository_id,scope,target,weight,state,created_at,updated_at) VALUES(?,?,?,'memory',?,1.0,'active','now','now')",
            graphicle_runtime::db::params![format!("hint-{tag}"), f.workspace, f.repo, id],
        )
        .unwrap();
        db.execute(
            "INSERT INTO gf_stale(workspace,kind,id,reason,since) VALUES(?,'memory',?,'test','now')",
            graphicle_runtime::db::params![f.workspace, id],
        )
        .unwrap();
    }
    drop(db);
    let named = [
        ("gf_served", &["served"][..]),
        ("gf_outcomes", &["targets"][..]),
        ("gf_hints", &["target"][..]),
        ("gf_stale", &["id"][..]),
        ("gf_dependencies", &["dependent_id"][..]),
    ];
    for (table, columns) in named {
        assert!(
            holding(&f, table, columns, &memory) > 0,
            "{table} names A's memory"
        );
    }

    let deleted = f
        .store
        .namespace_delete(&f.workspace, &f.owner, "dev-a")
        .unwrap();
    let receipt = &deleted["receipt"];
    // The receipt's own scan of every table finds nothing, and the
    // project's vector backend is told to clear, so the receipt waits on it.
    assert_eq!(receipt["verified_local"], true, "{receipt:#}");
    assert_eq!(
        receipt["residue"]["references"]["matches"],
        json!([]),
        "{receipt:#}"
    );
    assert!(receipt["residue"]["references"]["tables_scanned"].as_u64() > Some(0));
    assert_eq!(receipt["external_pending"], true, "{receipt:#}");
    assert_eq!(receipt["complete"], false, "{receipt:#}");
    assert_eq!(
        receipt["external_cleanup"][0]["kind"], "qdrant",
        "{receipt:#}"
    );
    assert_eq!(
        receipt["namespace"]["credentials_revoked"], 2,
        "{receipt:#}"
    );
    let queued = f
        .store
        .read(|db| {
            graphicle_runtime::store::rows(
                db,
                "SELECT kind,state,reason FROM am_projection_cleanup WHERE scope=?",
                [&f.repo],
            )
        })
        .unwrap();
    assert!(
        queued
            .iter()
            .any(|c| c["kind"] == "qdrant" && c["reason"] == "memory_namespace_deleted"),
        "{queued:#?}"
    );
    // Independently: nothing names A's memory; B's records stay.
    for (table, columns) in named {
        assert_eq!(
            holding(&f, table, columns, &memory),
            0,
            "{table} still names A's memory"
        );
    }
    for (table, columns) in &named[..4] {
        assert!(
            holding(&f, table, columns, &b_memory) > 0,
            "{table} lost B's record"
        );
    }
    assert_eq!(
        f.store
            .namespace_stats(&f.workspace, &f.owner, "dev-a")
            .unwrap()["rows"],
        0
    );
    assert!(
        f.store
            .namespace_stats(&f.workspace, &f.owner, "dev-b")
            .unwrap()["rows"]
            .as_u64()
            > Some(0)
    );
    // Every key of the namespace is revoked; B's still works.
    for key in [&a, &reader] {
        assert!(f.command(key, "memories", json!({})).is_err());
    }
    assert!(
        f.command(&b, "memories", json!({}))
            .unwrap()
            .to_string()
            .contains(&b_memory)
    );
    // The receipt verifies later, and an unknown namespace is refused.
    let verified = graphicle_runtime::deletion::verify(
        &f.store,
        &f.workspace,
        &f.owner,
        &json!({"receipt_id":receipt["id"]}),
    )
    .unwrap();
    assert_eq!(verified["verified_local"], true, "{verified:#}");
    assert_eq!(
        verified["current"]["reappeared_memories"], 0,
        "{verified:#}"
    );
    assert!(
        f.store
            .namespace_delete(&f.workspace, &f.owner, "never-made")
            .is_err()
    );
}

/// A namespaced `memory.delete` makes the namespace's jobs and cached query
/// vectors stale, as a shared one does every job and cache: a job's result
/// may quote the deleted memory, so `workflow.status` no longer serves it.
/// Another namespace's jobs stay.
#[tokio::test]
async fn a_namespaced_memory_delete_leaves_no_stale_job() {
    let server = providers::start().await;
    let f = Fixture::new();
    let config = f.models(&server);
    let a = f.key("agent", Some("dev-a"), Some(true));
    let b = f.key("agent", Some("dev-b"), Some(true));
    let memory = f.remember(&a, &a, "a", "Deploys use the blue pipeline, says A.");
    f.remember(&b, &b, "b", "Deploys use the blue pipeline, says B.");
    f.run(&a, &config, json!({"kind":"embed","max_calls":2}))
        .await;
    let recall = json!({"kind":"recall","question":QUESTION,"max_calls":2});
    let a_job = f.run(&a, &config, recall.clone()).await;
    let b_job = f.run(&b, &config, recall).await;
    assert_eq!(a_job["state"], "complete", "{a_job:#}");
    let cached = |namespace: &str| {
        f.store
            .read(|db| {
                Ok(db.query_row(
                    "SELECT count(*) FROM am_query_cache WHERE repository_id=? AND memory_namespace=?",
                    graphicle_runtime::db::params![f.repo, namespace],
                    |row| row.get::<_, i64>(0),
                )?)
            })
            .unwrap()
    };
    assert!(cached("dev-a") > 0 && cached("dev-b") > 0);
    let status = |key: &Key, job: &Value| {
        f.store.job_status(
            &f.workspace,
            &key.capability,
            &f.args(json!({"job_id":job["id"]})),
        )
    };
    assert!(status(&a, &a_job).is_ok());

    f.command(&a, "memory.delete", json!({"memory_id":memory}))
        .unwrap();
    // A's job is gone, from the lookup and from the list.
    assert_eq!(status(&a, &a_job).unwrap_err().message, "Job unavailable");
    let listed = f.command(&a, "workflow.status", json!({})).unwrap();
    assert_eq!(listed["jobs"], json!([]), "{listed:#}");
    assert_eq!(cached("dev-a"), 0);
    // B's job and cache are not A's to clear.
    assert_eq!(status(&b, &b_job).unwrap()["state"], "complete");
    assert!(cached("dev-b") > 0);
}

/// Source retention runs for the whole project, and forgetting a document
/// removes the memories of every namespace that cite it. So a document a
/// namespace's reviewed memory cites is kept, as one shared memory cites is,
/// when the owner runs the policy; and since the plan counts every
/// namespace's memory, only an admin previews it.
#[test]
fn retention_keeps_what_a_namespace_cites_and_only_admins_preview_it() {
    let f = Fixture::new();
    let connector = s(
        &f.store
            .connector_add(
                &f.workspace,
                &f.owner,
                &f.args(json!({"kind":"jsonl","name":"chat","config":{"acl":{"workspace_readers":true,"credentials":[]}}})),
            )
            .unwrap(),
        "id",
    )
    .to_string();
    let quote = "The staging password rotates on the first of each month.";
    f.store
        .import_documents(
            &f.workspace,
            &f.owner,
            &f.args(json!({"connector_id":connector,"documents":[
                {"id":"old","title":"Old","content":"user: In 2019 planning was in Bristol.\n","source_timestamp":"2019-05-01T10:00:00Z"},
                {"id":"cited","title":"Cited","content":format!("user: {quote}\n"),"source_timestamp":"2019-06-01T10:00:00Z"}
            ]})),
        )
        .unwrap();
    let a = f.key("agent", Some("dev-a"), Some(true));
    let proposed = f
        .command(
            &a,
            "memory.propose",
            json!({"kind":"decision","title":"Password rotation","body":"The staging password rotates monthly.",
                "idempotency_key":"rotation","evidence":[{"source_id":format!("connector:{connector}:cited"),"quote":quote}]}),
        )
        .unwrap();
    let memory = s(&proposed, "memory_id").to_string();
    f.command(
        &a,
        "learning.review",
        json!({"memory_id":memory,"expected_revision":1,"decision":"approve","maintainer":"self","reason":"self review"}),
    )
    .unwrap();
    let policy = f.args(
        json!({"sources":{"connector_ids":[connector],"max_age_days":365,"by":"source_timestamp"}}),
    );

    // The plan is the owner's view: a namespaced key and a shared reader
    // are refused it.
    let reader = f.key("reader", None, None);
    for key in [&a, &reader] {
        assert_eq!(
            f.store
                .retention_preview(&f.workspace, &key.capability, &policy)
                .unwrap_err()
                .status,
            403
        );
    }
    let preview = f
        .store
        .retention_preview(&f.workspace, &f.owner, &policy)
        .unwrap();
    assert_eq!(preview["documents"], 1, "{preview:#}");
    assert_eq!(preview["skipped"]["cited_by_reviewed"], 1, "{preview:#}");
    let mut apply = policy.clone();
    apply["plan_hash"] = preview["plan_hash"].clone();
    let applied = f
        .store
        .maintain(&f.workspace, &f.owner, "retention.apply", &apply)
        .unwrap();
    assert_eq!(applied["forgotten_documents"], 1, "{applied:#}");
    // The cited document, and so A's memory, are kept.
    assert!(
        f.command(&a, "memories", json!({}))
            .unwrap()
            .to_string()
            .contains(&memory)
    );
}

/// R1: A namespaced key cannot mint a memory under a chosen `memory_id`
/// (which previously both acted as an existence oracle against another
/// namespace's id and let `memory.delete` run unscoped substring scans against
/// other namespaces' rows), and `memory.delete` scopes its reference cleanup
/// and receipt residue scan to the caller's namespace.
#[test]
fn namespaced_note_cannot_probe_or_target_another_namespace_by_memory_id() {
    let f = Fixture::new();
    let a = f.key("agent", Some("dev-a"), Some(true));
    let b = f.key("agent", Some("dev-b"), Some(true));
    let a_mem = f.remember(&a, &a, "a", "ORCA9912 secret deploy rule.");

    // Calling `note` with A's memory_id gets the exact same error as an
    // unknown id, both with and without `expected_revision`.
    let note_no_rev = |id: &str| {
        f.command(
            &b,
            "note",
            json!({"memory_id":id,"title":"Probe","body":"Trying to probe A."}),
        )
    };
    assert_unavailable(
        "note without expected_revision",
        note_no_rev(&a_mem),
        note_no_rev("no-such-memory"),
    );
    let note_with_rev = |id: &str| {
        f.command(
            &b,
            "note",
            json!({"memory_id":id,"expected_revision":1,"title":"Probe","body":"Trying to probe A."}),
        )
    };
    assert_unavailable(
        "note with expected_revision",
        note_with_rev(&a_mem),
        note_with_rev("no-such-memory"),
    );
    // Custom identifiers cannot be minted at all.
    assert_eq!(note_no_rev("ORCA9912").unwrap_err().status, 400);

    // Even when another namespace holds text containing B's legitimate memory
    // id (in a memory or cached answer), B's `memory.delete` neither deletes
    // A's cache entry nor reports A's row in B's deletion receipt.
    let b_mem = f.remember(&b, &b, "b", "B's ordinary note.");
    f.remember(
        &a,
        &a,
        "a-mentions-b-id",
        &format!("Unrelated note mentioning {b_mem}."),
    );
    let db = f.store.connect().unwrap();
    db.execute(
        "INSERT INTO am_model_cache(repository_id,request_hash,source_state,result,created_at,memory_namespace) VALUES(?,?,?,?,?,?)",
        graphicle_runtime::db::params![
            &f.repo,
            "a-cached-hash",
            "s",
            format!("{{\"quote\":\"{b_mem}\"}}"),
            "2026-01-01T00:00:00Z",
            "dev-a"
        ],
    )
    .unwrap();
    let deleted = f
        .command(&b, "memory.delete", json!({"memory_id": b_mem}))
        .unwrap();
    assert_eq!(deleted["receipt"]["verified_local"], true, "{deleted:#}");
    assert_eq!(
        deleted["receipt"]["residue"]["references"]["matches"],
        json!([]),
        "{deleted:#}"
    );
    let a_cache_kept: i64 = db
        .query_row(
            "SELECT count(*) FROM am_model_cache WHERE repository_id=? AND request_hash='a-cached-hash' AND memory_namespace='dev-a'",
            [&f.repo],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(a_cache_kept, 1, "B's memory.delete must not wipe A's cache");
}

/// R2: `freshness` reported in `status`, `context` and `graph.source` is
/// scoped to the caller's namespace, so stale memories, embeddings, cached
/// answers and revoked principals in one namespace never leak into another.
#[test]
fn freshness_counts_are_scoped_per_namespace() {
    let f = Fixture::new();
    let a = f.key("agent", Some("dev-a"), Some(true));
    let b = f.key("agent", Some("dev-b"), Some(true));
    let shared = f.key("reader", None, None);

    let a_mem = f.remember(&a, &a, "a-fresh", "Deploys go through the blue pipeline.");
    // Editing `notes.txt` marks A's cited memory stale in `gf_stale`.
    f.sync(&[
        (
            "notes.txt",
            "Deploys go through the green pipeline.\nRollbacks need two approvals.\n",
        ),
        ("deploy.py", CODE),
    ]);
    jobs::drain_projection(&f.store, &f.repo).unwrap();

    // Also plant a stale embedding, answer and principal belonging to A.
    let db = f.store.connect().unwrap();
    let a_hash: String = db
        .query_row(
            "SELECT capability_hash FROM credentials WHERE workspace_id=? AND memory_namespace='dev-a' LIMIT 1",
            [&f.workspace],
            |r| r.get(0),
        )
        .unwrap();
    db.execute(
        "INSERT INTO am_model_cache(repository_id,request_hash,source_state,result,created_at,memory_namespace) VALUES(?,?,?,?,?,?)",
        graphicle_runtime::db::params![
            &f.repo,
            "a-stale-ans",
            "s",
            "{}",
            "2026-01-01T00:00:00Z",
            "dev-a"
        ],
    )
    .unwrap();
    for (kind, id) in [
        ("embedding", format!("{}:memory:{a_mem}", f.repo)),
        ("answer", format!("{}:a-stale-ans", f.repo)),
        ("principal", a_hash),
    ] {
        db.execute(
            "INSERT INTO gf_stale(workspace,kind,id,since,reason) VALUES(?,?,?,?,?) ON CONFLICT DO NOTHING",
            graphicle_runtime::db::params![&f.workspace, kind, id, "2026-01-01T00:00:00Z", "test"],
        )
        .unwrap();
    }

    let a_status = f.command(&a, "status", json!({})).unwrap();
    assert!(
        a_status["freshness"]["by_kind"]["memory"]
            .as_u64()
            .unwrap_or(0)
            >= 1,
        "A should see its own stale memory: {a_status:#}"
    );
    assert_eq!(
        a_status["freshness"]["by_kind"]["embedding"], 1,
        "{a_status:#}"
    );
    assert_eq!(
        a_status["freshness"]["by_kind"]["answer"], 1,
        "{a_status:#}"
    );
    assert_eq!(
        a_status["freshness"]["by_kind"]["principal"], 1,
        "{a_status:#}"
    );

    let episode = s(&a_status["graph"], "episode").to_string();
    for (who, key) in [("B", &b), ("shared", &shared)] {
        for (op, args) in [
            ("status", json!({})),
            (
                "context",
                json!({"question":"blue pipeline","graph_retrieval":false}),
            ),
            ("context", json!({"question":"deploy"})),
            (
                "graph.source",
                json!({"episode":episode,"path":"notes.txt","start_line":1}),
            ),
        ] {
            let reply = f.command(key, op, args).unwrap();
            let by_kind = &reply["freshness"]["by_kind"];
            for kind in ["memory", "embedding", "answer", "principal"] {
                assert!(
                    by_kind.get(kind).is_none(),
                    "{who} via {op} saw A's stale {kind}: {reply:#}"
                );
            }
        }
    }
}

/// R3: The owner cannot retry a private namespace's failed or cancelled job
/// under the owner's shared credential, which would execute the namespace's
/// job payload in shared memory.
#[test]
fn owner_cannot_retry_a_namespaced_job_into_shared_memory() {
    let f = Fixture::new();
    let a = f.key("agent", Some("dev-a"), Some(true));
    let db = f.store.connect().unwrap();
    let a_hash: String = db
        .query_row(
            "SELECT capability_hash FROM credentials WHERE workspace_id=? AND memory_namespace='dev-a' LIMIT 1",
            [&f.workspace],
            |r| r.get(0),
        )
        .unwrap();
    db.execute(
        "INSERT INTO am_jobs(id,repository_id,kind,input_hash,state,payload,principal_hash,attempt,max_attempts,not_before,created_at,updated_at,memory_namespace) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)",
        graphicle_runtime::db::params![
            "job-private-failed",
            &f.repo,
            "recall",
            "ih-private-failed",
            "failed",
            "{\"question\":\"secret private question\"}",
            &a_hash,
            1,
            3,
            "2026-01-01T00:00:00Z",
            "2026-01-01T00:00:00Z",
            "2026-01-01T00:00:00Z",
            "dev-a"
        ],
    )
    .unwrap();

    let config: model::Config = serde_json::from_value(json!({
        "default_model": "fixture",
        "models": {
            "fixture": {
                "protocol": "chat",
                "base_url": "http://127.0.0.1:1/v1",
                "model": "fixture",
                "input_limit": 1000
            }
        }
    }))
    .unwrap();
    let err = f
        .store
        .retry_job(
            &f.workspace,
            &f.owner,
            &f.args(json!({"job_id":"job-private-failed"})),
            &config,
        )
        .unwrap_err();
    assert_eq!(err.message, "Job unavailable");
    let shared_recall_jobs: i64 = db
        .query_row(
            "SELECT count(*) FROM am_jobs WHERE repository_id=? AND kind='recall' AND memory_namespace IS NULL",
            [&f.repo],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        shared_recall_jobs, 0,
        "owner retry must not enqueue a shared job"
    );

    // The namespace's own key can still retry its failed job inside `dev-a`.
    let retried = f
        .store
        .retry_job(
            &f.workspace,
            &a.capability,
            &f.args(json!({"job_id":"job-private-failed"})),
            &config,
        )
        .unwrap();
    let new_id = s(&retried, "id");
    let ns: Option<String> = db
        .query_row(
            "SELECT memory_namespace FROM am_jobs WHERE id=?",
            [new_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(ns.as_deref(), Some("dev-a"));
}

/// R5: Deleting a namespace with more than 66 memories (15 text columns on
/// `am_jobs` -> >1000 `OR` terms) must not exceed SQLite's expression-depth
/// limit (`SQLITE_MAX_EXPR_DEPTH = 1000`).
#[test]
fn namespace_delete_handles_more_than_expression_depth_limit_memories() {
    let f = Fixture::new();
    let bulk = f.key("agent", Some("bulk-ns"), Some(true));
    for i in 0..75 {
        f.command(
            &bulk,
            "memory.propose",
            json!({
                "kind": "note",
                "title": format!("Bulk note {i}"),
                "body": format!("Bulk body {i}."),
                "idempotency_key": format!("bulk-{i}"),
                "evidence": [{"source_id": "file:notes.txt", "quote": "Deploys go through the blue pipeline."}]
            }),
        )
        .unwrap();
    }
    let deleted = f
        .store
        .namespace_delete(&f.workspace, &f.owner, "bulk-ns")
        .unwrap();
    assert!(
        deleted["deleted"].as_u64().unwrap_or(0) >= 75,
        "{deleted:#}"
    );
    assert_eq!(deleted["remaining"], 0, "{deleted:#}");
    assert_eq!(deleted["receipt"]["verified_local"], true, "{deleted:#}");
}

/// R6: Superseding a claim rewrites the revision journal; neither the stored
/// revision nor `memory.history` / historical `memory.search` may expose
/// `memory_namespace`.
#[test]
fn memory_revisions_conceal_memory_namespace_on_supersede_and_history() {
    let f = Fixture::new();
    let a = f.key("agent", Some("dev-a"), Some(true));
    let ids = f.write_as(&a, "sup");
    let prepared = f
        .command(
            &a,
            "interpretation.prepare",
            json!({"source_id":"file:notes.txt"}),
        )
        .unwrap();
    let statement = "Rollbacks need two approvals.";
    let supersede = json!({
        "statement": "Rollbacks need two approvals, updated.",
        "subject": "Rollbacks",
        "relation": "need",
        "object": "two approvals",
        "speaker": "document",
        "modality": "requirement",
        "conditions": [],
        "action": "SUPERSEDE",
        "target_id": ids["claim"],
        "valid_at": null,
        "invalid_at": null,
        "time_basis": "unknown",
        "anchor_id": null,
        "evidence": [{"id":"file:notes.txt","quote":statement}]
    });
    let proposed = f
        .command(
            &a,
            "interpretation.propose",
            json!({"source_id":"file:notes.txt","context_sha256":prepared["sha256"],"claims":[supersede]}),
        )
        .unwrap();
    let new_mem = s(&proposed["drafts"][0], "memory_id").to_string();
    f.command(
        &a,
        "learning.review",
        json!({"memory_id":new_mem,"expected_revision":1,"decision":"approve","maintainer":"self","reason":"supersede"}),
    )
    .unwrap();

    let history = f
        .command(&a, "memory.history", json!({"memory_id": new_mem}))
        .unwrap();
    assert!(
        !has_json_key(&history, "memory_namespace"),
        "memory.history leaked memory_namespace: {history:#}"
    );
    let as_of = f
        .command(
            &a,
            "memory.search",
            json!({"question":"Rollbacks","known_at":"2099-01-01T00:00:00Z"}),
        )
        .unwrap();
    assert!(
        !has_json_key(&as_of, "memory_namespace"),
        "memory.search known_at leaked memory_namespace: {as_of:#}"
    );
    let db = f.store.connect().unwrap();
    let stored: String = db
        .query_row(
            "SELECT data FROM repo_memory_revisions WHERE memory_id=? ORDER BY revision DESC LIMIT 1",
            [&new_mem],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        !stored.contains("memory_namespace"),
        "repo_memory_revisions stored memory_namespace: {stored}"
    );
}

/// R7: Saving a memory in namespace A does not evict namespace B's
/// `am_query_cache`, and deleting a shared memory does not wipe any private
/// namespace's `am_jobs`, `am_vectors`, `am_model_cache` or `am_query_cache`.
#[test]
fn memory_writes_and_shared_deletes_preserve_other_namespaces_caches() {
    let f = Fixture::new();
    let a = f.key("agent", Some("dev-a"), Some(true));
    let b = f.key("agent", Some("dev-b"), Some(true));
    let b_mem = f.remember(&b, &b, "b-keep", "B's memory stays cached.");

    let db = f.store.connect().unwrap();
    db.execute(
        "INSERT INTO am_query_cache(repository_id,request_hash,result,memory_namespace) VALUES(?,?,?,?)",
        graphicle_runtime::db::params![&f.repo, "b-q-1", "[1.0,0.0]", "dev-b"],
    )
    .unwrap();
    db.execute(
        "INSERT INTO am_model_cache(repository_id,request_hash,source_state,result,created_at,memory_namespace) VALUES(?,?,?,?,?,?)",
        graphicle_runtime::db::params![&f.repo, "b-m-1", "s", "{}", "2026-01-01T00:00:00Z", "dev-b"],
    )
    .unwrap();
    db.execute(
        "INSERT INTO am_vectors(repository_id,identity,source_sha,model_key,dimensions,vector,memory_namespace) VALUES(?,?,?,?,?,?,?)",
        graphicle_runtime::db::params![
            &f.repo,
            format!("memory:{b_mem}"),
            "sha",
            "m",
            2,
            "[1.0,0.0]",
            "dev-b"
        ],
    )
    .unwrap();
    db.execute(
        "INSERT INTO am_jobs(id,repository_id,kind,input_hash,state,payload,principal_hash,attempt,max_attempts,not_before,created_at,updated_at,memory_namespace) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)",
        graphicle_runtime::db::params![
            "b-job-keep",
            &f.repo,
            "recall",
            "ih-b-job-keep",
            "complete",
            "{}",
            "h",
            1,
            1,
            "2026-01-01T00:00:00Z",
            "2026-01-01T00:00:00Z",
            "2026-01-01T00:00:00Z",
            "dev-b"
        ],
    )
    .unwrap();
    db.execute(
        "INSERT INTO am_query_cache(repository_id,request_hash,result,memory_namespace) VALUES(?,?,?,NULL)",
        graphicle_runtime::db::params![&f.repo, "shared-q-1", "[1.0,0.0]"],
    )
    .unwrap();

    // A writing a memory must not evict B's or shared `am_query_cache`.
    f.remember(&a, &a, "a-save", "A saves a memory.");
    let b_q: i64 = db
        .query_row(
            "SELECT count(*) FROM am_query_cache WHERE repository_id=? AND memory_namespace='dev-b'",
            [&f.repo],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(b_q, 1, "A's memory save evicted B's am_query_cache");

    // Shared memory.delete clears shared caches while leaving B intact.
    let shared_owner = Key {
        capability: f.owner.clone(),
    };
    let s_mem = f.remember(
        &shared_owner,
        &shared_owner,
        "shared-del",
        "Temporary shared memory.",
    );
    f.command(&shared_owner, "memory.delete", json!({"memory_id": s_mem}))
        .unwrap();
    for (table, expected) in [
        ("am_query_cache", 1i64),
        ("am_model_cache", 1),
        ("am_vectors", 1),
        ("am_jobs", 1),
    ] {
        let count: i64 = db
            .query_row(
                &format!("SELECT count(*) FROM {table} WHERE repository_id=? AND memory_namespace='dev-b'"),
                [&f.repo],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, expected, "shared memory.delete wiped B's {table}");
    }
    let shared_q: i64 = db
        .query_row(
            "SELECT count(*) FROM am_query_cache WHERE repository_id=? AND memory_namespace IS NULL",
            [&f.repo],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        shared_q, 0,
        "shared memory.delete should clear shared am_query_cache"
    );
}

/// R8: Read-only mode refuses a SQLite data folder that lacks the format-2
/// namespace schema (e.g. a released 0.1.19 store with the same package
/// version, or an interrupted migration) instead of opening and failing
/// mid-query with `no such column`.
#[test]
fn read_only_open_refuses_store_missing_namespace_columns() {
    if graphicle_runtime::db::postgres_configured() {
        eprintln!("skipped: read-only mode opens SQLite data folders only");
        return;
    }
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let data = dir.path().join("data");
    std::fs::create_dir_all(&home).unwrap();
    let store = Store::open(&data).unwrap();
    {
        let db = store.connect().unwrap();
        db.execute(
            "DELETE FROM store_readers WHERE reason='memory_namespaces'",
            [],
        )
        .unwrap();
    }
    drop(store);

    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_graphicle"));
    crate::child_env::home(&mut command, &home);
    command
        .env("GRAPHICLE_NO_UPDATE_NOTIFIER", "1")
        .env("GRAPHICLE_READ_ONLY", "1")
        .arg("--data-dir")
        .arg(&data)
        .args(["status", "--json"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let out = crate::fork_guard::spawn(&mut command)
        .unwrap()
        .wait_with_output()
        .unwrap();
    assert!(
        !out.status.success(),
        "read-only mode should refuse a store missing the format-2 namespace schema"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("upgraded") || stderr.contains("read-only"),
        "{stderr}"
    );
}

/// R9: `namespace_delete` removes `gf_dependencies` and `gf_stale` entries for
/// the namespace's cached `ANSWER` artifacts and wiki `SUMMARY` articles,
/// without touching another namespace's entries.
#[test]
fn namespace_delete_clears_answer_and_wiki_invalidation_state() {
    let f = Fixture::new();
    let _a = f.key("agent", Some("dev-a"), Some(true));
    let _b = f.key("agent", Some("dev-b"), Some(true));
    let db = f.store.connect().unwrap();
    for (ns, hash) in [("dev-a", "ans-a"), ("dev-b", "ans-b")] {
        db.execute(
            "INSERT INTO am_model_cache(repository_id,request_hash,source_state,result,created_at,memory_namespace) VALUES(?,?,?,?,?,?)",
            graphicle_runtime::db::params![&f.repo, hash, "s", "{}", "2026-01-01T00:00:00Z", ns],
        )
        .unwrap();
        let answer_id = format!("{}:{hash}", f.repo);
        let wiki_id = format!("wiki:{}:namespace:{ns}:core", f.repo);
        for (kind, id) in [("answer", answer_id), ("summary", wiki_id)] {
            db.execute(
                "INSERT INTO gf_dependencies(workspace,source_kind,source_id,dependent_kind,dependent_id,recorded_at) VALUES(?,?,?,?,?,?)",
                graphicle_runtime::db::params![&f.workspace, "file", format!("{}:notes.txt", f.repo), kind, &id, "2026-01-01T00:00:00Z"],
            )
            .unwrap();
            db.execute(
                "INSERT INTO gf_stale(workspace,kind,id,since,reason) VALUES(?,?,?,?,?)",
                graphicle_runtime::db::params![
                    &f.workspace,
                    kind,
                    &id,
                    "2026-01-01T00:00:00Z",
                    "changed"
                ],
            )
            .unwrap();
        }
    }

    f.store
        .namespace_delete(&f.workspace, &f.owner, "dev-a")
        .unwrap();

    for table in ["gf_dependencies", "gf_stale"] {
        let col = if table == "gf_dependencies" {
            "dependent_id"
        } else {
            "id"
        };
        let a_left: i64 = db
            .query_row(
                &format!("SELECT count(*) FROM {table} WHERE workspace=? AND ({col}=? OR {col}=?)"),
                graphicle_runtime::db::params![
                    &f.workspace,
                    format!("{}:ans-a", f.repo),
                    format!("wiki:{}:namespace:dev-a:core", f.repo)
                ],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(a_left, 0, "{table} kept A's answer/wiki invalidation rows");
        let b_left: i64 = db
            .query_row(
                &format!("SELECT count(*) FROM {table} WHERE workspace=? AND ({col}=? OR {col}=?)"),
                graphicle_runtime::db::params![
                    &f.workspace,
                    format!("{}:ans-b", f.repo),
                    format!("wiki:{}:namespace:dev-b:core", f.repo)
                ],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(b_left, 2, "{table} lost B's answer/wiki invalidation rows");
    }
}

/// R10: Namespace export -> delete -> import rebuilds derived dependency and
/// symbol-pin indexes, updates `last_write_at`, preserves deterministic export
/// digests, and rejects case-insensitive rowid overrides or cross-namespace
/// secondary foreign keys.
#[test]
fn namespace_restore_rebuilds_derived_indexes_and_rejects_bad_archive_links() {
    let f = Fixture::new();
    let a = f.key("agent", Some("dev-a"), Some(true));
    let b = f.key("agent", Some("dev-b"), Some(true));
    let b_mem = f.remember(&b, &b, "b-fk", "B's memory for FK rejection.");
    // Create a memory in A that cites `notes.txt` and an unprojected code file
    // (`pin_restore.py`), then project so `gf_symbol_pins` is populated.
    let pin_code = "def restore_helper():\n    return 99\n";
    f.sync(&[
        ("notes.txt", NOTES),
        ("deploy.py", CODE),
        ("pin_restore.py", pin_code),
    ]);
    let proposed = f
        .command(
            &a,
            "memory.propose",
            json!({
                "kind": "decision",
                "title": "Deploy pipeline pin",
                "body": "restore_helper returns 99.",
                "idempotency_key": "a-pin-1",
                "evidence": [
                    {"source_id": "file:notes.txt", "quote": "Deploys go through the blue pipeline."},
                    {"source_id": "file:pin_restore.py", "quote": "    return 99"}
                ]
            }),
        )
        .unwrap();
    let a_mem = s(&proposed, "memory_id").to_string();
    f.command(
        &a,
        "learning.review",
        json!({"memory_id":a_mem,"expected_revision":1,"decision":"approve","maintainer":"self","reason":"ok"}),
    )
    .unwrap();
    jobs::drain_projection(&f.store, &f.repo).unwrap();
    let exported = f
        .store
        .namespace_export(&f.workspace, &f.owner, "dev-a")
        .unwrap();

    f.store
        .namespace_delete(&f.workspace, &f.owner, "dev-a")
        .unwrap();

    // Tampered archive with uppercase ROWID is rejected.
    let mut bad_rowid = exported.clone();
    bad_rowid["archive"]["tables"]["repo_memories"][0]["ROWID"] = json!(999);
    bad_rowid["sha256"] = json!(graphicle_runtime::digest(&bad_rowid["archive"]));
    assert_eq!(
        f.store
            .namespace_import(&f.workspace, &f.owner, "dev-a", &bad_rowid)
            .unwrap_err()
            .status,
        400
    );

    // Tampered archive linking an `am_claims` row to B's `memory_id` is rejected.
    let mut bad_claim = exported.clone();
    bad_claim["archive"]["tables"]["am_claims"] = json!([{
        "id": "forged-claim",
        "repository_id": f.repo,
        "memory_id": b_mem,
        "state": "active",
        "data": "{}",
        "source_state": "s",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-01T00:00:00Z",
        "memory_namespace": "dev-a"
    }]);
    bad_claim["sha256"] = json!(graphicle_runtime::digest(&bad_claim["archive"]));
    assert_eq!(
        f.store
            .namespace_import(&f.workspace, &f.owner, "dev-a", &bad_claim)
            .unwrap_err()
            .status,
        400
    );

    // Tampered archive linking an `am_model_calls` row to a job in B is rejected.
    let db = f.store.connect().unwrap();
    db.execute(
        "INSERT INTO am_jobs(id,repository_id,kind,input_hash,state,payload,principal_hash,attempt,max_attempts,not_before,created_at,updated_at,memory_namespace) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)",
        graphicle_runtime::db::params![
            "b-job-fk",
            &f.repo,
            "recall",
            "ih-b-job-fk",
            "complete",
            "{}",
            "h",
            1,
            1,
            "2026-01-01T00:00:00Z",
            "2026-01-01T00:00:00Z",
            "2026-01-01T00:00:00Z",
            "dev-b"
        ],
    )
    .unwrap();
    let mut bad_call = exported.clone();
    bad_call["archive"]["tables"]["am_model_calls"] = json!([{
        "id": "forged-call",
        "repository_id": f.repo,
        "job_id": "b-job-fk",
        "provider": "openai",
        "model": "m",
        "request_hash": "rh",
        "input_tokens": 1,
        "output_tokens": 1,
        "elapsed_ms": 1,
        "cached": 0,
        "created_at": "2026-01-01T00:00:00Z",
        "memory_namespace": "dev-a"
    }]);
    bad_call["sha256"] = json!(graphicle_runtime::digest(&bad_call["archive"]));
    assert_eq!(
        f.store
            .namespace_import(&f.workspace, &f.owner, "dev-a", &bad_call)
            .unwrap_err()
            .status,
        400
    );

    // Valid import succeeds, rebuilds `gf_dependencies`, updates `last_write_at`,
    // and re-exports with the identical SHA-256.
    let before_write: Option<String> = db
        .query_row(
            "SELECT last_write_at FROM am_memory_namespaces WHERE workspace_id=? AND name='dev-a'",
            [&f.workspace],
            |r| r.get(0),
        )
        .unwrap();
    f.store
        .namespace_import(&f.workspace, &f.owner, "dev-a", &exported)
        .unwrap();
    let deps: i64 = db
        .query_row(
            "SELECT count(*) FROM gf_dependencies WHERE workspace=? AND dependent_kind='memory' AND dependent_id=?",
            graphicle_runtime::db::params![&f.workspace, &a_mem],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        deps > 0,
        "restore should rebuild gf_dependencies for restored memories"
    );
    let pins: i64 = db
        .query_row(
            "SELECT count(*) FROM gf_symbol_pins WHERE repository_id=? AND memory_id=? AND state='pinned'",
            graphicle_runtime::db::params![&f.repo, &a_mem],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        pins, 1,
        "restore should rebuild gf_symbol_pins for restored memories"
    );
    let after_write: Option<String> = db
        .query_row(
            "SELECT last_write_at FROM am_memory_namespaces WHERE workspace_id=? AND name='dev-a'",
            [&f.workspace],
            |r| r.get(0),
        )
        .unwrap();
    assert!(after_write.is_some() && after_write >= before_write);
    let reexported = f
        .store
        .namespace_export(&f.workspace, &f.owner, "dev-a")
        .unwrap();
    assert_eq!(reexported["sha256"], exported["sha256"]);
}

/// R14: `symbol_links::attach` scopes `gf_symbol_pins` joins by
/// `memory_namespace`, so a symbol pinned by namespace A is never attached for
/// namespace B or a shared reader.
#[test]
fn symbol_links_attach_isolates_pinned_memories_by_namespace() {
    let f = Fixture::new();
    let a = f.key("agent", Some("dev-a"), Some(true));
    let b = f.key("agent", Some("dev-b"), Some(true));
    let pin_code = "def helper():\n    return 42\n";
    // Sync a new code file without projecting first so the memory's quote
    // enters `gf_symbol_pins` as `pending` and is pinned by projection.
    f.sync(&[
        ("notes.txt", NOTES),
        ("deploy.py", CODE),
        ("pin_target.py", pin_code),
    ]);
    let proposed = f
        .command(
            &a,
            "memory.propose",
            json!({
                "kind": "decision",
                "title": "Helper constant",
                "body": "helper returns 42.",
                "idempotency_key": "a-pin-helper",
                "evidence": [{"source_id": "file:pin_target.py", "quote": "    return 42"}]
            }),
        )
        .unwrap();
    let a_mem = s(&proposed, "memory_id").to_string();
    f.command(
        &a,
        "learning.review",
        json!({"memory_id":a_mem,"expected_revision":1,"decision":"approve","maintainer":"self","reason":"ok"}),
    )
    .unwrap();
    jobs::drain_projection(&f.store, &f.repo).unwrap();

    let db = f.store.connect().unwrap();
    let pinned: i64 = db
        .query_row(
            "SELECT count(*) FROM gf_symbol_pins WHERE repository_id=? AND memory_id=? AND state='pinned'",
            graphicle_runtime::db::params![&f.repo, &a_mem],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pinned, 1, "projection should pin A's pending quote");
    let stable_id: String = db
        .query_row(
            "SELECT stable_id FROM gf_symbol_heads WHERE repository_id=? AND relpath='pin_target.py' LIMIT 1",
            [&f.repo],
            |r| r.get(0),
        )
        .unwrap();

    let attached_for = |key: &Key| {
        f.store
            .authorized_read(&f.workspace, &key.capability, "read", |db, p| {
                let repository = graphicle_runtime::store::repo(db, p, &f.repo)?;
                let mut nodes = vec![json!({"stable_id": stable_id})];
                graphicle_runtime::symbol_links::attach(db, p, &repository, &mut nodes)?;
                Ok(json!(nodes))
            })
            .unwrap()
    };
    let a_attached = attached_for(&a);
    assert!(
        a_attached.to_string().contains(&a_mem),
        "A should see its own pinned memory: {a_attached:#?}"
    );
    let b_attached = attached_for(&b);
    assert!(
        !b_attached.to_string().contains(&a_mem),
        "B saw A's pinned memory via gf_symbol_pins: {b_attached:#?}"
    );
}

/// An archive larger than the server's default body limit (24 MiB), but
/// within the 64 MiB `import` restores, reaches `import` over HTTP: the
/// route's own limit applies, so `export` never makes an archive that
/// `import` cannot take.
#[tokio::test]
async fn an_archive_over_the_default_body_limit_reaches_import() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    let f = Fixture::new();
    f.key("agent", Some("dev-a"), None);
    let app = graphicle_runtime::api::router(
        graphicle_runtime::api::App::new(
            f.store.clone(),
            model::Config::default(),
            vec!["localhost".into()],
        )
        .unwrap(),
    );
    let send = |bytes: usize| {
        let app = app.clone();
        let body =
            json!({"archive":{"format":"not-an-archive","padding":"x".repeat(bytes)},"sha256":"0"});
        let request = Request::builder()
            .method("POST")
            .uri(format!(
                "/api/workspaces/{}/memory-namespaces/dev-a/import",
                f.workspace
            ))
            .header("authorization", format!("Bearer {}", f.owner))
            .header("host", "localhost")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        async move { app.oneshot(request).await.unwrap().status().as_u16() }
    };
    // Read and refused for what it is (not an archive), not for its size.
    assert_eq!(send(30 * 1024 * 1024).await, 400);
    // Beyond the archive limit and its envelope, the body itself is refused.
    assert_eq!(send(70 * 1024 * 1024).await, 413);
}

/// R13: Deleting a memory removes the scope's `am_jobs` so job results can no
/// longer quote the deleted memory, but detaches `am_model_calls.job_id` first
/// so `ON DELETE CASCADE` never erases billable token-usage metering records.
#[test]
fn memory_delete_preserves_model_call_metering_records() {
    let f = Fixture::new();
    let a = f.key("agent", Some("dev-a"), Some(true));
    let a_mem = f.remember(&a, &a, "a-bill", "A's metered memory.");
    let shared_owner = Key {
        capability: f.owner.clone(),
    };
    let s_mem = f.remember(
        &shared_owner,
        &shared_owner,
        "s-bill",
        "Shared metered memory.",
    );

    let db = f.store.connect().unwrap();
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let now_ts = chrono::Utc::now().to_rfc3339();
    for (job_id, call_id, ns, in_tok, out_tok) in [
        ("job-a-bill", "call-a-bill", Some("dev-a"), 120i64, 45i64),
        ("job-s-bill", "call-s-bill", None, 200i64, 80i64),
    ] {
        db.execute(
            "INSERT INTO am_jobs(id,repository_id,kind,input_hash,state,payload,principal_hash,attempt,max_attempts,not_before,created_at,updated_at,memory_namespace) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)",
            graphicle_runtime::db::params![
                job_id,
                &f.repo,
                "recall",
                format!("ih-{job_id}"),
                "complete",
                "{}",
                "h",
                1,
                1,
                &now_ts,
                &now_ts,
                &now_ts,
                ns
            ],
        )
        .unwrap();
        db.execute(
            "INSERT INTO am_model_calls(id,repository_id,job_id,provider,model,request_hash,input_tokens,output_tokens,elapsed_ms,cached,created_at,memory_namespace) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",
            graphicle_runtime::db::params![
                call_id,
                &f.repo,
                job_id,
                "openai",
                "gpt-test",
                format!("rh-{call_id}"),
                in_tok,
                out_tok,
                10,
                0,
                &now_ts,
                ns
            ],
        )
        .unwrap();
    }

    // Namespaced `memory.delete` removes A's job but keeps A's model call row.
    f.command(&a, "memory.delete", json!({"memory_id": a_mem}))
        .unwrap();
    let a_jobs: i64 = db
        .query_row(
            "SELECT count(*) FROM am_jobs WHERE id='job-a-bill'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(a_jobs, 0);
    let a_call_job: Option<String> = db
        .query_row(
            "SELECT job_id FROM am_model_calls WHERE id='call-a-bill'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(a_call_job, None);

    // Shared `memory.delete` removes the shared job but keeps its model call row.
    f.command(&shared_owner, "memory.delete", json!({"memory_id": s_mem}))
        .unwrap();
    let s_jobs: i64 = db
        .query_row(
            "SELECT count(*) FROM am_jobs WHERE id='job-s-bill'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(s_jobs, 0);
    let s_call_job: Option<String> = db
        .query_row(
            "SELECT job_id FROM am_model_calls WHERE id='call-s-bill'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(s_call_job, None);

    // Metering still sees all 320 input and 125 output tokens across both calls.
    let usage =
        graphicle_runtime::metering::model_usage(&db, &f.workspace, &today, &today).unwrap();
    assert_eq!(usage["by_model"][0]["calls"], 2, "{usage:#}");
    assert_eq!(usage["by_model"][0]["input_tokens"], 320, "{usage:#}");
    assert_eq!(usage["by_model"][0]["output_tokens"], 125, "{usage:#}");
}

/// R15a: `vector::publish` uploads only `reviewed` private namespace memory
/// vectors to Qdrant; draft or retired private `am_vectors` rows are never
/// published.
#[tokio::test]
async fn vector_publish_skips_draft_and_retired_private_memories() {
    let server = providers::start().await;
    let f = Fixture::new();
    let config = f.models(&server);
    let a = f.key("agent", Some("dev-a"), Some(true));

    let rev_id = f.remember(&a, &a, "rev", "Reviewed memory for Qdrant.");
    let draft = f
        .command(
            &a,
            "memory.propose",
            json!({
                "kind": "note",
                "title": "Draft memory",
                "body": "Unreviewed draft must not publish.",
                "idempotency_key": "draft-qdrant",
                "evidence": [{"source_id": "file:notes.txt", "quote": "Deploys go through the blue pipeline."}]
            }),
        )
        .unwrap();
    let draft_id = s(&draft, "memory_id").to_string();
    let ret_id = f.remember(&a, &a, "ret", "Retired memory must not publish.");
    f.command(
        &a,
        "memory.state",
        json!({"memory_id": ret_id, "expected_revision": 2, "state": "retired"}),
    )
    .unwrap();

    // Embed (which creates vectors for source + reviewed memory), then also
    // plant `am_vectors` rows for the draft and retired memories.
    let embedded = f
        .run(&a, &config, json!({"kind":"embed","max_calls":2}))
        .await;
    let model_key = s(&embedded["result"], "model_key").to_string();
    let db = f.store.connect().unwrap();
    for id in [&draft_id, &ret_id] {
        db.execute(
            "INSERT INTO am_vectors(repository_id,identity,source_sha,model_key,dimensions,vector,memory_namespace) VALUES(?,?,?,?,?,?,?)",
            graphicle_runtime::db::params![
                &f.repo,
                format!("memory:{id}"),
                "sha",
                &model_key,
                3,
                "[1.0,0.0,0.0]",
                "dev-a"
            ],
        )
        .unwrap();
    }

    graphicle_runtime::vector::configure(
        &f.store,
        &f.workspace,
        &f.owner,
        &f.args(json!({"endpoint":format!("http://{}/qdrant",server.address),"collection":"reviewed-only"})),
    )
    .unwrap();
    let client = reqwest::Client::new();
    graphicle_runtime::vector::publish(
        &f.store,
        &client,
        &f.workspace,
        &f.owner,
        &f.args(json!({"model_key": model_key})),
    )
    .await
    .unwrap();

    let published: std::collections::BTreeSet<String> = server
        .calls
        .points
        .lock()
        .unwrap()
        .values()
        .map(|p| s(&p["payload"], "identity").to_string())
        .collect();
    assert!(
        published.contains(&format!("memory:{rev_id}")),
        "reviewed private memory should publish: {published:?}"
    );
    assert!(
        !published.contains(&format!("memory:{draft_id}")),
        "draft private memory must not publish: {published:?}"
    );
    assert!(
        !published.contains(&format!("memory:{ret_id}")),
        "retired private memory must not publish: {published:?}"
    );
}

/// R15c: On PostgreSQL (READ COMMITTED), a concurrent `erase` holding
/// `FOR UPDATE` on `am_memory_namespaces` serializes against an in-flight
/// `Store::authorized` write that already passed its first `authorize_hash`:
/// once `erase` commits and revokes the key, the blocked write re-checks
/// authorization and fails with 403 without writing any row.
#[test]
fn postgres_namespace_delete_serializes_concurrent_writes() {
    if !graphicle_runtime::db::postgres_configured() {
        eprintln!("skipped: PostgreSQL READ COMMITTED row-lock serialization test");
        return;
    }
    let f = Fixture::new();
    let a = f.key("agent", Some("race-ns"), Some(true));
    let db = f.store.connect().unwrap();
    let cred_id: String = db
        .query_row(
            "SELECT id FROM credentials WHERE workspace_id=? AND memory_namespace='race-ns' LIMIT 1",
            [&f.workspace],
            |r| r.get(0),
        )
        .unwrap();

    // Open a second connection simulating `erase`: acquire `FOR UPDATE` on
    // `am_memory_namespaces` and revoke the key inside an uncommitted tx.
    let mut eraser = f.store.connect().unwrap();
    let tx = eraser.transaction().unwrap();
    let locked: i64 = tx
        .query_row(
            "SELECT 1 FROM am_memory_namespaces WHERE workspace_id=? AND name='race-ns' FOR UPDATE",
            [&f.workspace],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(locked, 1);
    tx.execute(
        "UPDATE credentials SET revoked_at=? WHERE id=?",
        graphicle_runtime::db::params![chrono::Utc::now().to_rfc3339(), &cred_id],
    )
    .unwrap();

    // Spawn a writer that enters `Store::authorized` BEFORE `tx` commits
    // (so its first `authorize_hash` sees `revoked_at IS NULL`), then blocks
    // on `UPDATE am_memory_namespaces`.
    let store = f.store.clone();
    let workspace = f.workspace.clone();
    let repo = f.repo.clone();
    let token = a.capability.clone();
    let started = std::sync::Arc::new(std::sync::Barrier::new(2));
    let started_bg = started.clone();
    let handle = std::thread::spawn(move || {
        let app = App::new(store, model::Config::default(), vec![]).unwrap();
        started_bg.wait();
        api::command(
            &app,
            &workspace,
            &token,
            "work_state.write",
            &json!({
                "repository_id": repo,
                "list": "race",
                "fields": {"task": "must-not-commit", "status": "todo"}
            }),
        )
    });

    started.wait();
    // Give the writer thread time to run its first `authorize_hash` and block
    // on `UPDATE am_memory_namespaces` behind our `FOR UPDATE` lock.
    std::thread::sleep(std::time::Duration::from_millis(120));
    tx.commit().unwrap();

    let err = handle
        .join()
        .unwrap()
        .expect_err("write racing erase must fail once key is revoked");
    assert_eq!(err.status, 403, "{err:?}");
    let rows_left: i64 = db
        .query_row(
            "SELECT count(*) FROM am_work_state WHERE repository_id=? AND memory_namespace='race-ns'",
            [&f.repo],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows_left, 0, "blocked write must not have inserted any row");
}
