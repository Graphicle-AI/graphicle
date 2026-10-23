//! Agent-session capture: Claude Code and Codex transcripts become connector
//! documents, one per completed turn, under an explicit policy (`<private>`
//! spans dropped, credentials redacted, large tool output elided, reasoning
//! never stored). Imports are idempotent, `session.forget` signs a receipt and
//! blocks the session's later turns, and `propose_memory` only ever writes a
//! source-checked draft. The hooks never block the client.
use graphicle_runtime::{
    api, capture, deletion, local, model,
    store::{Store, s},
};
use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Stdio};
use tempfile::TempDir;

const CLAUDE_SESSION: &str = "5c0ffee0-1111-4222-8333-944445555666";
const CODEX_SESSION: &str = "0199aaaa-bbbb-7ccc-8ddd-eeeeffff0000";

struct Fixture {
    dir: TempDir,
    store: Store,
    session: local::Session,
    root: std::path::PathBuf,
    repo: String,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path().join("store")).unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir_all(root.join("deploy")).unwrap();
        std::fs::write(
            root.join("deploy/bucket.tf"),
            "resource \"bucket\" \"assets\" {\n  region = \"us-east-1\"\n}\n",
        )
        .unwrap();
        let mut session = local::session(&store, true).unwrap();
        local::sync(&store, &mut session, &root, None).unwrap();
        let repo = session.project.clone().unwrap();
        Self {
            dir,
            store,
            session,
            root,
            repo,
        }
    }
    fn ws(&self) -> &str {
        &self.session.workspace
    }
    fn key(&self) -> &str {
        &self.session.capability
    }
    fn args(&self, mut args: Value) -> Value {
        args["repository_id"] = json!(self.repo);
        args
    }
    fn capture(&self, client: capture::Client, transcript: &str) -> Value {
        self.store
            .capture_transcript(self.ws(), self.key(), &self.repo, client, transcript)
            .unwrap()
    }
    /// `(id, content and metadata)` of every row of a connector table.
    fn stored(&self, table: &str) -> Vec<(String, String)> {
        let db = self.store.connect().unwrap();
        let mut statement = db
            .prepare(&format!(
                "SELECT id,content||' '||metadata FROM {table} ORDER BY id"
            ))
            .unwrap();
        statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }
    fn source_id(&self, connector: &str, document: &str) -> String {
        format!("connector:{connector}:{document}")
    }
    fn propose(&self, args: Value) -> graphicle_runtime::Result<Value> {
        self.store
            .project_mutate(self.ws(), self.key(), "memory.propose", &self.args(args))
    }
    fn memories(&self) -> Vec<Value> {
        self.store
            .project_read(self.ws(), self.key(), "memories", &self.args(json!({})))
            .unwrap()
            .as_array()
            .cloned()
            .unwrap_or_default()
    }
}

fn lines(records: &[Value]) -> String {
    records
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Two Claude Code turns; the second ends in an open tool call unless
/// `finished`.
fn claude(cwd: &str, finished: bool) -> String {
    let record = |kind: &str, at: &str, message: Value| json!({"type":kind,"sessionId":CLAUDE_SESSION,"cwd":cwd,"gitBranch":"main","timestamp":at,"message":message});
    let mut records = vec![
        record(
            "user",
            "2026-09-30T10:00:00Z",
            json!({"role":"user","content":"Why does the deploy fail? <private>my badge number is 4417-2290</private>"}),
        ),
        record(
            "assistant",
            "2026-09-30T10:00:05Z",
            json!({"role":"assistant","content":[
                {"type":"thinking","thinking":"secret chain of thought"},
                {"type":"text","text":"Checking the deploy config."},
                {"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"deploy/config.yaml"}}]}),
        ),
        record(
            "user",
            "2026-09-30T10:00:06Z",
            json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1",
                "content":"region: eu-west-1\napi_key=sk-live-0123456789abcdefghij"}]}),
        ),
        record(
            "assistant",
            "2026-09-30T10:00:09Z",
            json!({"role":"assistant","content":[{"type":"text","text":"The deploy targets eu-west-1 but the bucket lives in us-east-1."}]}),
        ),
        record(
            "user",
            "2026-09-30T10:01:00Z",
            json!({"role":"user","content":"Move the bucket to eu-west-1."}),
        ),
        record(
            "assistant",
            "2026-09-30T10:01:04Z",
            json!({"role":"assistant","content":[{"type":"tool_use","id":"toolu_2","name":"Edit",
                "input":{"file_path":"deploy/bucket.tf","old_string":"us-east-1","new_string":"eu-west-1"}}]}),
        ),
    ];
    // A sub-agent's records never become turns.
    records.push(json!({"type":"user","sessionId":CLAUDE_SESSION,"isSidechain":true,"message":{"role":"user","content":"sub-agent prompt"}}));
    if finished {
        records.push(record(
            "user",
            "2026-09-30T10:01:05Z",
            json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_2","content":"Edited deploy/bucket.tf"}]}),
        ));
        records.push(record(
            "assistant",
            "2026-09-30T10:01:08Z",
            json!({"role":"assistant","content":[{"type":"text","text":"The bucket now lives in eu-west-1 with the deploy."}]}),
        ));
    }
    lines(&records)
}

fn codex(cwd: &str) -> String {
    let record = |at: &str, kind: &str, payload: Value| json!({"timestamp":at,"type":kind,"payload":payload});
    lines(&[
        record(
            "2026-09-30T11:00:00Z",
            "session_meta",
            json!({"id":CODEX_SESSION,"cwd":cwd,"git":{"branch":"main"}}),
        ),
        record(
            "2026-09-30T11:00:00Z",
            "response_item",
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>\n  <cwd>/elsewhere</cwd>\n</environment_context>"}]}),
        ),
        record(
            "2026-09-30T11:00:01Z",
            "response_item",
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":"Run the unit tests"}]}),
        ),
        record(
            "2026-09-30T11:00:02Z",
            "response_item",
            json!({"type":"reasoning","summary":[{"type":"summary_text","text":"hidden reasoning summary"}],"encrypted_content":"gAAAAB"}),
        ),
        record(
            "2026-09-30T11:00:03Z",
            "response_item",
            json!({"type":"function_call","name":"shell","arguments":"{\"command\":[\"cargo\",\"test\"]}","call_id":"call_1"}),
        ),
        record(
            "2026-09-30T11:00:09Z",
            "response_item",
            json!({"type":"function_call_output","call_id":"call_1","output":"{\"output\":\"test result: ok. 3 passed\",\"metadata\":{\"exit_code\":0}}"}),
        ),
        record(
            "2026-09-30T11:00:10Z",
            "response_item",
            json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"All 3 unit tests pass."}]}),
        ),
        record(
            "2026-09-30T11:00:10Z",
            "event_msg",
            json!({"type":"agent_message","message":"All 3 unit tests pass."}),
        ),
    ])
}

#[test]
fn claude_code_turns_are_captured_once_under_the_policy() {
    let f = Fixture::new();
    let cwd = f.root.to_string_lossy().into_owned();
    let first = f.capture(capture::Client::ClaudeCode, &claude(&cwd, false));
    assert_eq!(
        (
            &first["turns_seen"],
            &first["turns_completed"],
            &first["imported"]
        ),
        (&json!(2), &json!(1), &json!(1)),
        "the turn waiting on a tool result is not complete: {first:#}"
    );
    assert_eq!(first["capture_policy"]["private_spans_dropped"], 1);
    assert_eq!(first["capture_policy"]["credentials_redacted"], 1);
    let connector = s(&first, "connector_id").to_string();
    let turn = format!("claude-code:{CLAUDE_SESSION}:0001");
    let docs = f.stored("am_connector_docs");
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0].0, turn);
    let text = &docs[0].1;
    for kept in [
        "Why does the deploy fail?",
        "Checking the deploy config.",
        "region: eu-west-1",
        "bucket lives in us-east-1",
    ] {
        assert!(text.contains(kept), "{kept} missing from {text}");
    }
    for dropped in [
        "4417-2290",
        "badge number",
        "sk-live",
        "secret chain of thought",
        "sub-agent prompt",
    ] {
        for table in [
            "am_connector_docs",
            "am_connector_versions",
            "am_connector_stage",
        ] {
            assert!(
                f.stored(table)
                    .iter()
                    .all(|(_, content)| !content.contains(dropped)),
                "{dropped} stored in {table}"
            );
        }
    }

    // The turn is an ordinary, readable, owner-only source.
    let source = f
        .store
        .read_project_source(
            f.ws(),
            f.key(),
            &f.args(json!({"source_id": f.source_id(&connector, &turn)})),
        )
        .unwrap();
    assert!(s(&source, "quote").contains("bucket lives in us-east-1"));
    assert_eq!(source["metadata"]["session_id"], CLAUDE_SESSION);
    assert_eq!(
        source["metadata"]["files_touched"],
        json!(["deploy/config.yaml"])
    );
    assert_eq!(
        source["metadata"]["capture_policy"]["private_spans"],
        "dropped"
    );
    let connectors = f
        .store
        .connector_list(f.ws(), f.key(), &f.args(json!({})))
        .unwrap();
    assert_eq!(connectors["connectors"][0]["name"], capture::CONNECTOR_NAME);
    assert_eq!(
        connectors["connectors"][0]["config"]["acl"]["workspace_readers"],
        false
    );

    // Re-import stores nothing new; the finished second turn is added once.
    let again = f.capture(capture::Client::ClaudeCode, &claude(&cwd, false));
    assert_eq!(
        (&again["imported"], &again["unchanged"]),
        (&json!(0), &json!(1)),
        "{again:#}"
    );
    let finished = f.capture(capture::Client::ClaudeCode, &claude(&cwd, true));
    assert_eq!(
        (&finished["imported"], &finished["unchanged"]),
        (&json!(1), &json!(1)),
        "{finished:#}"
    );
    assert_eq!(f.stored("am_connector_docs").len(), 2);
    assert_eq!(f.connector_count(), 1, "one connector per project");

    // Timeline: the latest session, oldest turn first.
    let timeline = f
        .store
        .session_timeline(f.ws(), f.key(), &f.args(json!({})))
        .unwrap();
    assert_eq!(timeline["session_id"], CLAUDE_SESSION, "{timeline:#}");
    assert_eq!(timeline["turns_in_session"], 2);
    assert_eq!(timeline["turns"][0]["turn"], 1);
    assert_eq!(
        timeline["turns"][1]["source_id"],
        f.source_id(&connector, &format!("claude-code:{CLAUDE_SESSION}:0002"))
    );
    let anchored = f
        .store
        .session_timeline(
            f.ws(),
            f.key(),
            &f.args(json!({"anchor_source_id": f.source_id(&connector, &turn), "before": 0, "after": 0})),
        )
        .unwrap();
    assert_eq!(anchored["turns"].as_array().unwrap().len(), 1);
    assert_eq!(anchored["turns"][0]["turn"], 1);
}

impl Fixture {
    fn connector_count(&self) -> i64 {
        self.store
            .connect()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM am_connectors WHERE repository_id=?",
                [&self.repo],
                |r| r.get(0),
            )
            .unwrap()
    }
}

#[test]
fn codex_rollouts_skip_preambles_and_reasoning() {
    let f = Fixture::new();
    let parsed = capture::parse_codex(&codex(&f.root.to_string_lossy())).unwrap();
    assert_eq!(parsed.session_id, CODEX_SESSION);
    assert_eq!(
        parsed.turns.len(),
        1,
        "the environment preamble is not a prompt"
    );
    let turn = &parsed.turns[0];
    assert_eq!(turn.prompt, "Run the unit tests");
    assert_eq!(turn.exit_codes, vec![0]);
    assert_eq!(turn.tools, vec!["shell".to_string()]);
    assert!(
        turn.text.contains("test result: ok. 3 passed"),
        "{}",
        turn.text
    );
    assert!(turn.text.contains("assistant: All 3 unit tests pass."));
    assert_eq!(
        turn.text.matches("All 3 unit tests pass.").count(),
        1,
        "event copies are not repeated"
    );
    for dropped in ["hidden reasoning", "gAAAAB", "environment_context"] {
        assert!(!turn.text.contains(dropped), "{dropped} kept");
    }
    let captured = f.capture(capture::Client::Codex, &codex(&f.root.to_string_lossy()));
    assert_eq!(captured["imported"], 1, "{captured:#}");
    assert_eq!(
        f.stored("am_connector_docs")[0].0,
        format!("codex:{CODEX_SESSION}:0001")
    );

    // Oversized tool output is replaced by its size and digest.
    let huge = "x".repeat(capture::TOOL_TEXT_CAP + 10);
    let rollout = codex(&f.root.to_string_lossy()).replace("test result: ok. 3 passed", &huge);
    let elided = capture::parse_codex(&rollout).unwrap();
    assert!(!elided.turns[0].text.contains(&huge));
    assert!(
        elided.turns[0]
            .text
            .contains(&format!("[tool output: {} bytes, sha256 ", huge.len()))
    );
    assert_eq!(elided.turns[0].applied.elided, 1);
}

/// claude-mem ffdff16: Codex's own helper threads (a task title, memory
/// consolidation, onboarding suggestions) are not a person's turns — neither
/// their prompts nor what answered them are captured, in a rollout's items or
/// in an older rollout's events. Codex only.
#[test]
fn codex_helper_threads_are_not_captured() {
    let f = Fixture::new();
    let cwd = f.root.to_string_lossy().to_string();
    let record = |at: &str, kind: &str, payload: Value| json!({"timestamp":at,"type":kind,"payload":payload});
    let meta = record(
        "2026-09-30T11:00:00Z",
        "session_meta",
        json!({"id":CODEX_SESSION,"cwd":cwd}),
    );
    let user = |at: &str, text: &str| {
        record(
            at,
            "response_item",
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":text}]}),
        )
    };
    let assistant = |at: &str, text: &str| {
        record(
            at,
            "response_item",
            json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}),
        )
    };
    let event = |at: &str, kind: &str, text: &str| {
        record(at, "event_msg", json!({"type":kind,"message":text}))
    };
    let helpers = [
        "You are a helpful assistant. You will be presented with a user prompt, and your job is to provide a short title for the task it starts.",
        "\n  ## Memory Writing Agent: Phase 2 (Consolidation)\n\nConsolidate the raw memories.",
        "# Overview\r\n\r\nGenerate 0 to 3 hyperpersonalized suggestions for what to try next.",
        "# Overview\nGenerate 0 to 3 hyperpersonalized suggestions for what to try next.",
    ];
    for helper in helpers {
        let alone = lines(&[
            meta.clone(),
            user("2026-09-30T11:00:01Z", helper),
            record(
                "2026-09-30T11:00:02Z",
                "response_item",
                json!({"type":"function_call","name":"shell","arguments":"{\"command\":[\"ls\"]}","call_id":"call_9"}),
            ),
            record(
                "2026-09-30T11:00:03Z",
                "response_item",
                json!({"type":"function_call_output","call_id":"call_9","output":"raw_memories.md"}),
            ),
            assistant("2026-09-30T11:00:04Z", "Deploy region fix"),
            event("2026-09-30T11:00:04Z", "user_message", helper),
            event("2026-09-30T11:00:04Z", "agent_message", "Deploy region fix"),
        ]);
        let parsed = capture::parse_codex(&alone).unwrap();
        assert!(parsed.turns.is_empty(), "{helper:?}: {:?}", parsed.turns);

        // Between a person's turns, the helper's answer joins neither.
        let mixed = lines(&[
            meta.clone(),
            user("2026-09-30T11:00:01Z", "Run the unit tests"),
            assistant("2026-09-30T11:00:02Z", "All 3 unit tests pass."),
            user("2026-09-30T11:00:03Z", helper),
            record(
                "2026-09-30T11:00:04Z",
                "response_item",
                json!({"type":"function_call","name":"shell","arguments":"{\"command\":[\"ls\"]}","call_id":"call_9"}),
            ),
            assistant("2026-09-30T11:00:05Z", "Deploy region fix"),
            user("2026-09-30T11:00:06Z", "Now fix the lint"),
            assistant("2026-09-30T11:00:07Z", "Lint is clean."),
        ]);
        let parsed = capture::parse_codex(&mixed).unwrap();
        let prompts: Vec<_> = parsed.turns.iter().map(|t| t.prompt.as_str()).collect();
        assert_eq!(prompts, ["Run the unit tests", "Now fix the lint"]);
        for turn in &parsed.turns {
            assert!(!turn.text.contains("Deploy region fix"), "{}", turn.text);
            assert!(turn.tools.is_empty(), "{:?}", turn.tools);
        }

        // An older rollout's events alone.
        let events = lines(&[
            meta.clone(),
            event("2026-09-30T11:00:01Z", "user_message", helper),
            event("2026-09-30T11:00:02Z", "agent_message", "Deploy region fix"),
            event("2026-09-30T11:00:03Z", "user_message", "Run the unit tests"),
            event(
                "2026-09-30T11:00:04Z",
                "agent_message",
                "All 3 unit tests pass.",
            ),
        ]);
        let parsed = capture::parse_codex(&events).unwrap();
        assert_eq!(parsed.turns.len(), 1, "{helper:?}: {:?}", parsed.turns);
        assert_eq!(parsed.turns[0].prompt, "Run the unit tests");
        assert!(!parsed.turns[0].text.contains("Deploy region fix"));
    }

    // Only a prompt that opens with a helper's first line is one.
    let quoted = lines(&[
        meta.clone(),
        user(
            "2026-09-30T11:00:01Z",
            "Why does Codex send '## Memory Writing Agent: Phase 2 (Consolidation)'?",
        ),
        assistant("2026-09-30T11:00:02Z", "It consolidates memories."),
    ]);
    assert_eq!(capture::parse_codex(&quoted).unwrap().turns.len(), 1);
    let claude = claude(&cwd, true).replace(
        "Move the bucket to eu-west-1.",
        "## Memory Writing Agent: Phase 2 (Consolidation) is a heading I wrote",
    );
    let parsed = capture::parse(capture::Client::ClaudeCode, &claude).unwrap();
    assert_eq!(parsed.turns.len(), 2, "{:?}", parsed.turns);
    assert!(
        parsed.turns[1]
            .prompt
            .starts_with("## Memory Writing Agent")
    );
}

/// claude-mem's protocol-payload check and 703ad99: a lone
/// `<task-notification>` block — what Claude Code sends when a background
/// task ends — starts no turn, so what answers it stays with the turn that
/// started the task; Codex's skill and plugin preambles are not prompts
/// either.
#[test]
fn protocol_notices_and_codex_skill_preambles_are_not_prompts() {
    let f = Fixture::new();
    let cwd = f.root.to_string_lossy().to_string();
    let notice = "<task-notification>\n<task-id>b7f2</task-id>\n<status>completed</status>\n<summary>Background command \"cargo build\" completed (exit code 0)</summary>\n</task-notification>";
    let said = |kind: &str, at: &str, message: Value| json!({"type":kind,"sessionId":CLAUDE_SESSION,"cwd":cwd,"timestamp":at,"message":message});
    let reply = |at: &str, text: &str| {
        said(
            "assistant",
            at,
            json!({"role":"assistant","content":[{"type":"text","text":text}]}),
        )
    };
    let transcript = lines(&[
        said(
            "user",
            "2026-09-30T10:00:00Z",
            json!({"role":"user","content":"Build the release in the background"}),
        ),
        reply("2026-09-30T10:00:02Z", "Started the build."),
        said(
            "user",
            "2026-09-30T10:03:00Z",
            json!({"role":"user","content":[{"type":"text","text":format!("\n{notice}\n")}]}),
        ),
        reply(
            "2026-09-30T10:03:02Z",
            "The release build finished cleanly.",
        ),
        said(
            "user",
            "2026-09-30T10:04:00Z",
            json!({"role":"user","content":format!("{notice}\nWhat did it print?")}),
        ),
        reply("2026-09-30T10:04:02Z", "It printed two warnings."),
    ]);
    let parsed = capture::parse(capture::Client::ClaudeCode, &transcript).unwrap();
    assert_eq!(parsed.turns.len(), 2, "{:?}", parsed.turns);
    assert_eq!(
        parsed.turns[0].prompt,
        "Build the release in the background"
    );
    assert!(
        parsed.turns[0]
            .text
            .contains("assistant: The release build finished cleanly."),
        "{}",
        parsed.turns[0].text
    );
    assert!(
        !parsed.turns[0].text.contains("b7f2"),
        "{}",
        parsed.turns[0].text
    );
    assert!(
        parsed.turns[1].prompt.ends_with("What did it print?"),
        "a notice with words after it is a prompt: {:?}",
        parsed.turns[1].prompt
    );

    let record = |at: &str, kind: &str, payload: Value| json!({"timestamp":at,"type":kind,"payload":payload});
    let meta = record(
        "2026-09-30T11:00:00Z",
        "session_meta",
        json!({"id":CODEX_SESSION,"cwd":cwd}),
    );
    let user = |at: &str, text: &str| {
        record(
            at,
            "response_item",
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":text}]}),
        )
    };
    let assistant = |at: &str, text: &str| {
        record(
            at,
            "response_item",
            json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}),
        )
    };
    let rollout = lines(&[
        meta.clone(),
        user(
            "2026-09-30T11:00:00Z",
            "<skills_instructions>\n## Skills\n- deploy: ships a build\n</skills_instructions>",
        ),
        user(
            "2026-09-30T11:00:00Z",
            "<recommended_plugins>\n- linear\n</recommended_plugins>",
        ),
        user("2026-09-30T11:00:01Z", "Run the unit tests"),
        assistant("2026-09-30T11:00:02Z", "Started them in the background."),
        user("2026-09-30T11:00:30Z", notice),
        assistant("2026-09-30T11:00:31Z", "All 3 unit tests pass."),
    ]);
    let parsed = capture::parse_codex(&rollout).unwrap();
    assert_eq!(parsed.turns.len(), 1, "{:?}", parsed.turns);
    assert_eq!(parsed.turns[0].prompt, "Run the unit tests");
    let text = &parsed.turns[0].text;
    assert!(text.contains("assistant: All 3 unit tests pass."), "{text}");
    for dropped in ["deploy: ships", "linear", "b7f2"] {
        assert!(!text.contains(dropped), "{dropped} kept: {text}");
    }

    let event = |at: &str, kind: &str, text: &str| {
        record(at, "event_msg", json!({"type":kind,"message":text}))
    };
    let events = lines(&[
        meta,
        event("2026-09-30T11:00:01Z", "user_message", "Run the unit tests"),
        event("2026-09-30T11:00:02Z", "agent_message", "Started them."),
        event("2026-09-30T11:00:30Z", "user_message", notice),
        event("2026-09-30T11:00:31Z", "agent_message", "All 3 pass."),
    ]);
    let parsed = capture::parse_codex(&events).unwrap();
    assert_eq!(parsed.turns.len(), 1, "{:?}", parsed.turns);
    assert!(parsed.turns[0].text.contains("assistant: All 3 pass."));
}

#[test]
fn forgetting_a_session_signs_a_receipt_and_blocks_its_later_turns() {
    let f = Fixture::new();
    let cwd = f.root.to_string_lossy().into_owned();
    let captured = f.capture(capture::Client::ClaudeCode, &claude(&cwd, true));
    assert_eq!(captured["imported"], 2);
    f.capture(capture::Client::Codex, &codex(&cwd));

    let forgotten = f
        .store
        .forget_session(
            f.ws(),
            f.key(),
            &f.args(json!({"client":"claude-code","session_id":CLAUDE_SESSION})),
        )
        .unwrap();
    assert_eq!(forgotten["forgotten_turns"], 2, "{forgotten:#}");
    assert_eq!(forgotten["later_turns_blocked"], true);
    let receipt = &forgotten["receipt"];
    assert_eq!(receipt["operation"], "source.forget");
    assert_eq!(receipt["verified_local"], true, "{receipt:#}");
    assert!(
        !receipt.to_string().contains(CLAUDE_SESSION),
        "receipts name documents only by hash"
    );
    let checked = deletion::verify(
        &f.store,
        f.ws(),
        f.key(),
        &json!({"receipt_id": receipt["id"]}),
    )
    .unwrap();
    assert_eq!(checked["complete"], true, "{checked:#}");

    let left = f.stored("am_connector_docs");
    assert_eq!(left.len(), 1, "the Codex session stays: {left:?}");
    assert!(left[0].0.starts_with("codex:"));
    for table in ["am_connector_docs", "am_connector_versions"] {
        assert!(
            f.stored(table)
                .iter()
                .all(|(_, content)| !content.contains("bucket lives in us-east-1"))
        );
    }

    // The transcript is still on disk and grows: nothing of it returns.
    let longer = format!(
        "{}\n{}\n{}",
        claude(&cwd, true),
        json!({"type":"user","sessionId":CLAUDE_SESSION,"timestamp":"2026-09-30T10:05:00Z","message":{"role":"user","content":"And the CDN?"}}),
        json!({"type":"assistant","sessionId":CLAUDE_SESSION,"timestamp":"2026-09-30T10:05:03Z","message":{"role":"assistant","content":[{"type":"text","text":"The CDN follows the bucket."}]}})
    );
    let again = f.capture(capture::Client::ClaudeCode, &longer);
    assert_eq!(
        (&again["imported"], &again["session_forgotten"]),
        (&json!(0), &json!(true)),
        "{again:#}"
    );
    assert_eq!(f.stored("am_connector_docs").len(), 1);
    // An in-flight capture may have prepared this batch before session.forget.
    // The import transaction must check the session tombstone too.
    let late = f.store.import_documents(f.ws(), f.key(), &f.args(json!({
        "connector_id":captured["connector_id"],
        "documents":[{"id":format!("claude-code:{CLAUDE_SESSION}:99"),"title":"Late turn","content":"This completed before the session was forgotten."}]
    }))).unwrap();
    assert_eq!(late["imported"], 0, "{late:#}");
    assert_eq!(late["forgotten_skipped"], 1);
    assert_eq!(f.stored("am_connector_docs").len(), 1);
    let timeline = f
        .store
        .session_timeline(f.ws(), f.key(), &f.args(json!({})))
        .unwrap();
    assert_eq!(timeline["session_id"], CODEX_SESSION);
}

#[test]
fn proposed_memory_is_a_checked_draft_until_reviewed_and_goes_with_its_source() {
    let f = Fixture::new();
    let cwd = f.root.to_string_lossy().into_owned();
    let captured = f.capture(capture::Client::ClaudeCode, &claude(&cwd, true));
    let connector = s(&captured, "connector_id").to_string();
    let turn = f.source_id(&connector, &format!("claude-code:{CLAUDE_SESSION}:0001"));
    let proposal = json!({"kind":"decision","title":"Assets bucket region","body":"The assets bucket must live in eu-west-1, next to the deploy.",
        "reason":"Found while fixing a failed deploy","files":["deploy/bucket.tf"],
        "evidence":[{"source_id":turn,"quote":"The deploy targets eu-west-1 but the bucket lives in us-east-1."}],"idempotency_key":"deploy-region-1"});
    let saved = f.propose(proposal.clone()).unwrap();
    assert_eq!(
        (&saved["state"], &saved["approval_required"]),
        (&json!("draft"), &json!(true)),
        "{saved:#}"
    );
    let memory = s(&saved, "memory_id").to_string();
    let replay = f.propose(proposal.clone()).unwrap();
    assert_eq!(
        (&replay["memory_id"], &replay["replayed"]),
        (&json!(memory), &json!(true))
    );
    let mut changed = proposal.clone();
    changed["body"] = json!("A different claim under the same key.");
    assert!(
        f.propose(changed)
            .unwrap_err()
            .message
            .contains("different proposal")
    );
    let mut invented = proposal.clone();
    invented["idempotency_key"] = json!("deploy-region-2");
    invented["evidence"][0]["quote"] = json!("The bucket was always in eu-west-1.");
    assert!(
        f.propose(invented)
            .unwrap_err()
            .message
            .contains("absent from the original source")
    );
    let mut short = proposal.clone();
    short["idempotency_key"] = json!("deploy-region-3");
    short["evidence"][0]["quote"] = json!("bucket");
    assert!(
        f.propose(short).is_err(),
        "quotes need at least 8 characters"
    );
    let mut unknown = proposal.clone();
    unknown["idempotency_key"] = json!("deploy-region-4");
    unknown["kind"] = json!("fact");
    assert!(f.propose(unknown).is_err());

    let memories = f.memories();
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0]["state"], "draft");
    assert_eq!(memories[0]["authority"], "agent_proposed_draft");
    assert_eq!(
        memories[0]["data"]["proposal"]["idempotency_key"],
        "deploy-region-1"
    );

    // Session start shows reviewed memory only.
    let reviewed = f
        .store
        .note(
            f.ws(),
            f.key(),
            &f.args(json!({"kind":"decision","title":"Deploys run from main","body":"Deploys run from the main branch only.",
                "evidence":[{"id":turn,"quote":"Why does the deploy fail?"}]})),
        )
        .unwrap();
    let context = f
        .store
        .session_start_context(f.ws(), f.key(), &f.repo)
        .unwrap();
    assert!(context.contains("Deploys run from main"), "{context}");
    assert!(context.contains(s(&reviewed, "id")));
    assert!(
        !context.contains("Assets bucket region"),
        "drafts never reach session start: {context}"
    );
    assert!(context.contains("Recent captured turns"));
    assert!(context.chars().count() <= capture::START_CONTEXT_CHARS);

    // A later proposal citing only a repository file was written while the
    // turns were visible: it goes too, like any model draft.
    let other = f
        .propose(json!({"kind":"note","title":"Bucket file","body":"The bucket is declared in deploy/bucket.tf.",
            "evidence":[{"source_id":"file:deploy/bucket.tf","quote":"resource \"bucket\" \"assets\""}],"idempotency_key":"bucket-file"}))
        .unwrap();
    f.store
        .forget_session(
            f.ws(),
            f.key(),
            &f.args(json!({"client":"claude-code","session_id":CLAUDE_SESSION})),
        )
        .unwrap();
    let left: Vec<String> = f
        .memories()
        .iter()
        .map(|m| s(m, "id").to_string())
        .collect();
    assert!(
        !left.contains(&memory),
        "the draft citing a forgotten turn is removed"
    );
    assert!(
        !left.contains(&s(&other, "memory_id").to_string()),
        "and drafts written while it was visible"
    );
}

#[test]
fn mcp_publishes_timeline_always_and_proposals_only_with_learning() {
    let f = Fixture::new();
    f.capture(
        capture::Client::ClaudeCode,
        &claude(&f.root.to_string_lossy(), true),
    );
    let names = |learning: bool| -> Vec<String> {
        api::mcp_tools(learning)["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| s(t, "name").to_string())
            .collect()
    };
    assert!(names(false).contains(&"session_timeline".to_string()));
    assert!(!names(false).contains(&"propose_memory".to_string()));
    assert!(names(true).contains(&"propose_memory".to_string()));

    let mut app = api::App::new(f.store.clone(), model::Config::default(), vec![]).unwrap();
    let call = |app: &api::App, name: &str, arguments: Value| {
        api::mcp(app, f.ws(), f.key(), &f.repo, &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments}}))
            .unwrap()
    };
    let timeline = call(&app, "session_timeline", json!({"before": 1}));
    assert_eq!(timeline["result"]["isError"], false, "{timeline}");
    assert_eq!(
        timeline["result"]["structuredContent"]["session_id"],
        CLAUDE_SESSION
    );
    let refused = call(&app, "propose_memory", json!({}));
    assert!(refused.get("error").is_some(), "{refused}");

    app.mcp_learning = true;
    let source = timeline["result"]["structuredContent"]["turns"][1]["source_id"].clone();
    let proposal = json!({"kind":"task_state","title":"Bucket move in progress","body":"The bucket region change is edited but not deployed.",
        "evidence":[{"source_id":source,"quote":"The bucket now lives in eu-west-1 with the deploy."}],"idempotency_key":"mcp-1"});
    let saved = call(&app, "propose_memory", proposal);
    assert_eq!(saved["result"]["isError"], false, "{saved}");
    assert_eq!(saved["result"]["structuredContent"]["state"], "draft");
    let bad = call(
        &app,
        "propose_memory",
        json!({"kind":"note","title":"t","body":"b","evidence":[],"idempotency_key":"mcp-2"}),
    );
    assert!(
        bad.get("error").is_some(),
        "the published schema needs evidence: {bad}"
    );
}

/// claude-mem 47a5151's work state: an agent's own to-do lists, published
/// only when enabled, cleaned by the capture policy and folded so the latest
/// value of each key wins.
#[test]
fn mcp_work_state_is_opt_in_cleaned_and_folded() {
    let f = Fixture::new();
    let names = |tools: Value| -> Vec<String> {
        tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| s(t, "name").to_string())
            .collect()
    };
    for tools in [api::mcp_tools(false), api::mcp_tools(true)] {
        assert!(!names(tools).iter().any(|n| n.starts_with("work_state")));
    }
    let published = api::mcp_tools_with(false, true);
    assert!(names(published.clone()).contains(&"work_state_write".to_string()));
    assert!(!names(published.clone()).contains(&"propose_memory".to_string()));
    let hint = |name: &str| {
        published["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == name)
            .unwrap()["annotations"]["readOnlyHint"]
            .clone()
    };
    assert_eq!(hint("work_state_read"), true);
    assert_eq!(hint("work_state_write"), false);

    let mut app = api::App::new(f.store.clone(), model::Config::default(), vec![]).unwrap();
    let rpc = |app: &api::App, method: &str, params: Value| {
        api::mcp(
            app,
            f.ws(),
            f.key(),
            &f.repo,
            &json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}),
        )
        .unwrap()
    };
    let call = |app: &api::App, name: &str, arguments: Value| {
        rpc(
            app,
            "tools/call",
            json!({"name":name,"arguments":arguments}),
        )
    };
    let fields = json!({"list":"release","fields":{"task":"tag v1","status":"doing"}});
    let refused = call(&app, "work_state_write", fields.clone());
    assert!(refused.get("error").is_some(), "{refused}");
    let instructions = |app: &api::App| {
        s(&rpc(app, "initialize", json!({}))["result"], "instructions").to_string()
    };
    assert!(!instructions(&app).contains("work_state_write"));

    app.mcp_work_state = true;
    assert!(instructions(&app).contains("work_state_write"));
    let token = format!("ghp_{}", "a1B2".repeat(9));
    let saved = call(
        &app,
        "work_state_write",
        json!({"list":"release","fields":{"task":"tag v1","status":"doing","note":format!("push with {token}<private>my notes</private>")}}),
    );
    assert_eq!(saved["result"]["isError"], false, "{saved}");
    let reply = &saved["result"]["structuredContent"];
    assert_eq!(reply["policy"]["credentials_redacted"], 1, "{reply}");
    assert_eq!(reply["policy"]["private_spans_dropped"], 1, "{reply}");
    let text = s(reply, "text");
    assert!(
        text.starts_with("Saved to \"release\". Open in it:\n- release\n  - [doing] tag v1 (note=push with [credential redacted]"),
        "{text}"
    );
    assert!(
        !text.contains("ghp_") && !text.contains("my notes"),
        "{text}"
    );
    // Only what changed is written; the note stays.
    let state = call(
        &app,
        "work_state_write",
        json!({"list":"release","fields":{"note":"ship after review"}}),
    );
    assert!(
        s(&state["result"]["structuredContent"], "text")
            .contains("- release: note=ship after review"),
        "{state}"
    );
    let done = call(
        &app,
        "work_state_write",
        json!({"list":"release","fields":{"task":"tag v1","status":"Done"}}),
    );
    let done = s(&done["result"]["structuredContent"], "text").to_string();
    assert!(!done.contains("tag v1"), "{done}");
    let read = |arguments: Value| {
        let reply = call(&app, "work_state_read", arguments);
        assert_eq!(reply["result"]["isError"], false, "{reply}");
        s(&reply["result"]["structuredContent"], "text").to_string()
    };
    assert!(!read(json!({})).contains("tag v1"));
    let all = read(json!({"include_closed":true}));
    assert!(
        all.contains("  - [Done] tag v1 (note=push with [credential redacted]"),
        "{all}"
    );
    assert_eq!(
        read(json!({"list":"other"})),
        "Nothing open in \"other\"; include_closed shows closed items too."
    );

    // The published contract holds: a value is a scalar, fields are not
    // empty, and no call names another repository.
    for arguments in [
        json!({"list":"release","fields":{}}),
        json!({"list":"release","fields":{"task":{"nested":1}}}),
        json!({"list":"release"}),
        json!({"list":"release","fields":{"a":1},"repository_id":"other"}),
        json!({"include_closed":"yes"}),
    ] {
        let name = if arguments.get("include_closed").is_some() {
            "work_state_read"
        } else {
            "work_state_write"
        };
        let bad = call(&app, name, arguments.clone());
        assert!(
            bad.get("error").is_some() || bad["result"]["isError"] == true,
            "{arguments}: {bad}"
        );
    }
}

#[test]
fn open_work_leads_the_session_start_context_until_closed() {
    let f = Fixture::new();
    let context = || {
        f.store
            .session_start_context(f.ws(), f.key(), &f.repo)
            .unwrap()
    };
    let write = |fields: Value| {
        f.store
            .work_state_write(
                f.ws(),
                f.key(),
                &f.args(json!({"list":"bucket move","fields":fields})),
            )
            .unwrap()
    };
    // Nothing reviewed, open or stale: the line on when to use Graphicle
    // alone.
    let before = context();
    assert_eq!(
        before,
        format!(
            "{}\n",
            graphicle_runtime::agent_instructions::SESSION_ROUTING
        )
    );
    write(json!({"task":"move bucket","status":"doing","region":"eu-west-1"}));
    let open = context();
    assert!(
        open.contains("Open work an agent recorded (work_state_write; not reviewed)"),
        "{open}"
    );
    assert!(
        open.contains("- bucket move\n  - [doing] move bucket (region=eu-west-1), updated "),
        "{open}"
    );
    assert!(open.contains("No reviewed memories yet."), "{open}");

    // Many long items keep to the section's share of the context.
    for n in 0..40 {
        write(json!({"task":format!("step {n}"),"status":"todo","detail":"x".repeat(1500)}));
    }
    let full = context();
    assert!(full.chars().count() <= capture::START_CONTEXT_CHARS);
    assert!(
        full.contains("\n- 39 more lines not shown; work_state_read lists them\n"),
        "{full}"
    );
    assert!(full.contains("No reviewed memories yet."), "{full}");

    write(json!({"task":"move bucket","status":"dropped"}));
    for n in 0..40 {
        write(json!({"task":format!("step {n}"),"status":"done"}));
    }
    let closed = context();
    assert!(!closed.contains("Open work"), "{closed}");
    assert_eq!(closed, before);
}

#[test]
fn work_state_goes_with_its_project() {
    let f = Fixture::new();
    f.store
        .work_state_write(
            f.ws(),
            f.key(),
            &f.args(json!({"list":"notes","fields":{"task":"one","status":"todo"}})),
        )
        .unwrap();
    let count = || -> i64 {
        f.store
            .connect()
            .unwrap()
            .query_row("SELECT count(*) FROM am_work_state", [], |row| row.get(0))
            .unwrap()
    };
    assert_eq!(count(), 1);
    let snapshot = f
        .store
        .project_status(f.ws(), f.key(), &f.args(json!({})))
        .unwrap()["repository"]["snapshot"]
        .clone();
    let deleted = f
        .store
        .maintain(
            f.ws(),
            f.key(),
            "project.delete",
            &f.args(json!({"expected_snapshot":snapshot})),
        )
        .unwrap();
    assert_eq!(deleted["receipt"]["verified_local"], true, "{deleted:#}");
    assert_eq!(count(), 0);
}

#[test]
fn work_state_cli_and_stdio_server_follow_the_flag() {
    let f = Fixture::new();
    let mcp = |extra: &[&str]| {
        let mut command = binary(&f);
        command.args(["mcp", "--project", &f.repo]).args(extra);
        let input = [
            json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"work_state_write","arguments":{"list":"cli","fields":{"task":"check flag","status":"doing"}}}}),
        ]
        .map(|r| r.to_string())
        .join("\n")
            + "\n";
        let output = run_with_input(command, &input);
        assert!(output.status.success(), "{output:?}");
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>()
    };
    let plain = mcp(&[]);
    assert!(!plain[0].to_string().contains("work_state_write"));
    assert!(plain[1].get("error").is_some(), "{}", plain[1]);
    let enabled = mcp(&["--work-state"]);
    assert!(enabled[0].to_string().contains("work_state_write"));
    assert_eq!(enabled[1]["result"]["isError"], false, "{}", enabled[1]);

    let mut command = binary(&f);
    command.args(["work-state", "--project", &f.repo]);
    let output = run_without_input(&mut command);
    assert!(output.status.success(), "{output:?}");
    let shown = String::from_utf8_lossy(&output.stdout);
    assert!(
        shown.starts_with("- cli\n  - [doing] check flag, updated "),
        "{shown}"
    );

    let config = f.dir.path().join("client.json");
    let mut command = binary(&f);
    command
        .args(["connect", "claude-code", "--work-state", "--config-path"])
        .arg(&config);
    let output = run_without_input(&mut command);
    assert!(output.status.success(), "{output:?}");
    let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(s(&reply, "access").contains("work-state"), "{reply}");
    let written: Value = serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
    let args = written["mcpServers"]["graphicle"]["args"]
        .as_array()
        .unwrap();
    assert_eq!(args.last().unwrap(), "--work-state", "{written:#}");
}

#[test]
fn mcp_recall_can_skip_what_the_connection_was_already_given() {
    let f = Fixture::new();
    f.capture(
        capture::Client::ClaudeCode,
        &claude(&f.root.to_string_lossy(), true),
    );
    let app = api::App::new(f.store.clone(), model::Config::default(), vec![]).unwrap();
    // The whole packet (`structured`), which these checks read.
    let request = |mut arguments: Value| {
        arguments["structured"] = json!(true);
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"recall_project","arguments":arguments}})
    };
    let packet = |reply: Value| {
        assert_eq!(reply["result"]["isError"], false, "{reply}");
        reply["result"]["structuredContent"].clone()
    };
    let question = "Where does the bucket live now?";
    let mut connection = api::McpSession::default();
    let first = packet(
        api::mcp_session(
            &app,
            f.ws(),
            f.key(),
            &f.repo,
            &request(
                json!({"question":question,"pack_order":"chronological","reading_guidance":true}),
            ),
            &mut connection,
        )
        .unwrap(),
    );
    assert_eq!(first["pack_order"], "chronological");
    assert!(s(&first, "markdown").contains("Reading conversations: what a user says"));
    let given: Vec<Value> = first["sources"].as_array().unwrap().clone();
    assert!(!given.is_empty(), "{first}");
    // Without skip_shown the connection's record changes nothing.
    let again = packet(
        api::mcp_session(
            &app,
            f.ws(),
            f.key(),
            &f.repo,
            &request(json!({"question":question})),
            &mut connection,
        )
        .unwrap(),
    );
    assert!(again.get("already_shown").is_none());
    assert_eq!(again["sources"].as_array().unwrap().len(), given.len());
    let skipped = packet(
        api::mcp_session(
            &app,
            f.ws(),
            f.key(),
            &f.repo,
            &request(json!({"question":question,"skip_shown":true})),
            &mut connection,
        )
        .unwrap(),
    );
    let held = skipped["already_shown"].as_array().unwrap();
    assert_eq!(held.len(), given.len(), "{skipped}");
    for source in &given {
        assert!(
            held.iter()
                .any(|h| h["id"] == source["id"] && h["byte_start"] == source["byte_start"])
        );
    }
    assert!(s(&skipped, "markdown").contains("Already in context: bytes"));
    // A stateless connection has nothing to skip; a malformed span is refused.
    let stateless = packet(
        api::mcp(
            &app,
            f.ws(),
            f.key(),
            &f.repo,
            &request(json!({"question":question,"skip_shown":true})),
        )
        .unwrap(),
    );
    assert!(stateless.get("already_shown").is_none());
    let refused = api::mcp(
        &app,
        f.ws(),
        f.key(),
        &f.repo,
        &request(json!({"question":question,"shown":[{"id":"x"}]})),
    )
    .unwrap();
    assert!(refused.get("error").is_some(), "{refused}");
}

fn binary(f: &Fixture) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_graphicle"));
    command.arg("--data-dir").arg(f.store.directory.as_path());
    command
}

/// `command.output()`; the process starts through `crate::fork_guard`.
fn run_without_input(command: &mut Command) -> std::process::Output {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::fork_guard::spawn(command)
        .unwrap()
        .wait_with_output()
        .unwrap()
}

fn run_with_input(command: Command, input: &str) -> std::process::Output {
    run_timed(command, input).0
}

/// The output, and the time from starting the process to collecting its
/// output. The wait for `crate::fork_guard` comes first and is not counted.
fn run_timed(mut command: Command, input: &str) -> (std::process::Output, std::time::Duration) {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (mut child, began) = {
        let _forking = crate::fork_guard::forking();
        let began = std::time::Instant::now();
        (command.spawn().unwrap(), began)
    };
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    (child.wait_with_output().unwrap(), began.elapsed())
}

/// `setup --prompt-context`'s hook: a code question gets `graphicle
/// context`'s Markdown packet as the prompt's additional context, within
/// the hook's timeout; a short prompt, a question in plain words, a folder
/// outside every project and bad input get nothing, and never fail.
#[test]
fn the_prompt_hook_adds_the_packet_for_a_code_question_only() {
    let f = Fixture::new();
    let cwd = f.root.join("deploy").to_string_lossy().into_owned();
    let hook = |input: &str| {
        let mut command = binary(&f);
        command.args(["hook", "claude-code", "prompt-context"]);
        run_timed(command, input)
    };
    let prompt = "Why does the `assets` bucket in deploy/bucket.tf use us-east-1?";
    let payload = json!({"session_id":CLAUDE_SESSION,"cwd":cwd,
        "hook_event_name":"UserPromptSubmit","prompt":prompt});
    let (output, took) = hook(&payload.to_string());
    assert!(output.status.success());
    let printed: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        printed["hookSpecificOutput"]["hookEventName"],
        "UserPromptSubmit"
    );
    let markdown = s(&printed["hookSpecificOutput"], "additionalContext");
    assert!(
        markdown.starts_with("Graphicle project evidence"),
        "{markdown}"
    );
    assert!(markdown.contains("deploy/bucket.tf"), "{markdown}");
    assert!(markdown.chars().count() <= graphicle_runtime::query::PUSH_CHARS);
    assert!(
        took < std::time::Duration::from_secs(graphicle_runtime::prompt_hook::HOOK_TIMEOUT),
        "{took:?}"
    );
    let outside = f.dir.path().to_string_lossy().into_owned();
    for quiet in [
        json!({"cwd":cwd,"prompt":"yes, go on"}).to_string(),
        json!({"cwd":cwd,"prompt":"What did we talk about in the last session?"}).to_string(),
        json!({"cwd":outside,"prompt":prompt}).to_string(),
        json!({"cwd":cwd}).to_string(),
        "not json".to_string(),
    ] {
        let (output, _) = hook(&quiet);
        assert!(output.status.success(), "{quiet}");
        assert!(
            output.stdout.is_empty(),
            "{quiet}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

/// The prompt hook asks a small project in full, also while its passage
/// index is not built (a store from an earlier version), and a project with
/// reviewed memory in full, so the packet holds its memories: neither gets
/// the quick packet or its note.
#[test]
fn the_prompt_hook_asks_small_projects_and_reviewed_memory_in_full() {
    let f = Fixture::new();
    let cwd = f.root.join("deploy").to_string_lossy().into_owned();
    let hook = |session: &str| {
        let mut command = binary(&f);
        command.args(["hook", "claude-code", "prompt-context"]);
        let payload = json!({"session_id":session,"cwd":cwd,"hook_event_name":"UserPromptSubmit",
            "prompt":"Why does the `assets` bucket in deploy/bucket.tf use us-east-1?"});
        let (output, _) = run_timed(command, &payload.to_string());
        assert!(output.status.success());
        let printed: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(printed.get("systemMessage").is_none(), "{printed}");
        s(&printed["hookSpecificOutput"], "additionalContext").to_string()
    };
    {
        let db = f.store.connect().unwrap();
        for table in graphicle_runtime::passage_index::TABLES {
            db.execute(
                &format!("DELETE FROM {table} WHERE repository_id=?"),
                [&f.repo],
            )
            .unwrap();
        }
    }
    assert!(hook("route-index").contains("deploy/bucket.tf"));
    f.store
        .note(
            f.ws(),
            f.key(),
            &f.args(json!({"kind":"decision","title":"Assets bucket region",
                "body":"The assets bucket stays in us-east-1, next to the CDN.",
                "evidence":[{"id":"file:deploy/bucket.tf","quote":"resource \"bucket\" \"assets\""}]})),
        )
        .unwrap();
    let markdown = hook("route-memory");
    assert!(markdown.contains("next to the CDN"), "{markdown}");
}

#[test]
fn hooks_add_context_at_start_and_import_after_stop_without_blocking() {
    let f = Fixture::new();
    let cwd = f.root.join("deploy").to_string_lossy().into_owned();
    let start = json!({"session_id":CLAUDE_SESSION,"cwd":cwd,"hook_event_name":"SessionStart","source":"startup"});
    // Nothing reviewed, open or stale yet: the hook prints the line on when
    // to use Graphicle, alone.
    let mut command = binary(&f);
    command.args(["hook", "claude-code", "session-start"]);
    let output = run_with_input(command, &start.to_string());
    assert!(output.status.success());
    let printed: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        s(&printed["hookSpecificOutput"], "additionalContext").trim_end(),
        graphicle_runtime::agent_instructions::SESSION_ROUTING
    );
    f.store
        .work_state_write(
            f.ws(),
            f.key(),
            &f.args(json!({"list":"bucket move","fields":{"task":"move bucket","status":"doing"}})),
        )
        .unwrap();
    let mut command = binary(&f);
    command.args(["hook", "claude-code", "session-start"]);
    let output = run_with_input(command, &start.to_string());
    assert!(output.status.success());
    let printed: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        printed["hookSpecificOutput"]["hookEventName"],
        "SessionStart"
    );
    assert!(
        s(&printed["hookSpecificOutput"], "additionalContext").contains("Graphicle project memory")
    );

    // A resumed Claude Code conversation already holds that context; clear,
    // compact and Codex starts still receive it, as in claude-mem.
    let resume = json!({"session_id":CLAUDE_SESSION,"cwd":cwd,"hook_event_name":"SessionStart","source":"resume"});
    let mut command = binary(&f);
    command.args(["hook", "claude-code", "session-start"]);
    let output = run_with_input(command, &resume.to_string());
    assert!(output.status.success());
    assert!(
        output.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    for source in ["clear", "compact"] {
        let mut command = binary(&f);
        command.args(["hook", "claude-code", "session-start"]);
        let input = json!({"cwd":cwd,"hook_event_name":"SessionStart","source":source});
        let output = run_with_input(command, &input.to_string());
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("Graphicle project memory"),
            "{source}"
        );
    }
    let mut command = binary(&f);
    command.args(["hook", "codex", "session-start"]);
    let output = run_with_input(command, &resume.to_string());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Graphicle project memory"));

    // Outside synced projects, and on malformed input: silent success.
    for (event, input) in [
        (
            "session-start",
            json!({"cwd":"/","hook_event_name":"SessionStart"}).to_string(),
        ),
        ("stop", "not json".to_string()),
        (
            "stop",
            json!({"transcript_path":"/nonexistent/x.jsonl"}).to_string(),
        ),
    ] {
        let mut command = binary(&f);
        command.args(["hook", "claude-code", event]);
        let output = run_with_input(command, &input);
        assert!(output.status.success(), "{event}: {output:?}");
        assert!(
            output.stdout.is_empty(),
            "{event}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    let transcript = f.dir.path().join("session.jsonl");
    std::fs::write(&transcript, claude(&f.root.to_string_lossy(), true)).unwrap();
    let stop = json!({"session_id":CLAUDE_SESSION,"transcript_path":transcript,"cwd":cwd,"hook_event_name":"Stop","stop_hook_active":false});
    let mut command = binary(&f);
    command.args(["hook", "claude-code", "stop"]);
    let (output, waited) = run_timed(command, &stop.to_string());
    assert!(output.status.success());
    // The import runs in a detached process.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while f.stored("am_connector_docs").len() < 2 {
        assert!(
            std::time::Instant::now() < deadline,
            "the background import never landed"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    assert!(
        waited < std::time::Duration::from_secs(10),
        "the hook returned after {waited:?}"
    );
}

/// What capture hooks and the command line serve is counted when the
/// program ends: as the local owner, by the agent's session (a digest of its
/// id, never the id) for a start context, and on the text served.
#[test]
fn hooks_and_the_command_line_count_what_they_serve() {
    let f = Fixture::new();
    let cwd = f.root.join("deploy").to_string_lossy().into_owned();
    let served = |channel: &str| {
        graphicle_runtime::store::rows(
            &f.store.connect().unwrap(),
            "SELECT credential_id,session,kind,calls,tokens FROM am_served_usage WHERE channel=? ORDER BY session",
            [channel],
        )
        .unwrap()
    };
    // Open work, so the start context holds more than its first line.
    f.store
        .work_state_write(
            f.ws(),
            f.key(),
            &f.args(json!({"list":"bucket move","fields":{"task":"move bucket","status":"doing"}})),
        )
        .unwrap();
    let start = json!({"session_id":CLAUDE_SESSION,"cwd":cwd,"hook_event_name":"SessionStart","source":"startup"});
    let mut command = binary(&f);
    command.args(["hook", "claude-code", "session-start"]);
    let output = run_with_input(command, &start.to_string());
    assert!(output.status.success());
    let printed: Value = serde_json::from_slice(&output.stdout).unwrap();
    let context = s(&printed["hookSpecificOutput"], "additionalContext").to_string();
    // A resumed conversation gets nothing, so nothing is counted; a start
    // without a session id is counted without one.
    let resume = json!({"session_id":CLAUDE_SESSION,"cwd":cwd,"hook_event_name":"SessionStart","source":"resume"});
    let mut command = binary(&f);
    command.args(["hook", "claude-code", "session-start"]);
    assert!(
        run_with_input(command, &resume.to_string())
            .stdout
            .is_empty()
    );
    let mut command = binary(&f);
    command.args(["hook", "codex", "session-start"]);
    run_with_input(command, &json!({"cwd":cwd}).to_string());
    let hooks = served("hook");
    assert_eq!(hooks.len(), 2, "{hooks:?}");
    assert_eq!(hooks[0]["session"], "");
    assert_eq!(
        hooks[1]["session"],
        graphicle_runtime::metering::session_label("claude-code", CLAUDE_SESSION)
    );
    assert_eq!(
        hooks[1]["tokens"].as_u64(),
        Some(graphicle_memory::text::count(&context) as u64)
    );
    for row in &hooks {
        assert_eq!(row["credential_id"], "owner");
        assert_eq!(row["kind"], "session_start");
        assert_eq!(row["calls"], 1);
    }
    assert!(!format!("{hooks:?}").contains(CLAUDE_SESSION));

    let mut command = binary(&f);
    command.args(["context", "Which region holds the assets bucket?"]);
    let output = run_without_input(&mut command);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let printed = String::from_utf8(output.stdout).unwrap();
    let cli = served("cli");
    assert_eq!(cli.len(), 1, "{cli:?}");
    assert_eq!(cli[0]["kind"], "context");
    assert_eq!(cli[0]["credential_id"], "owner");
    assert_eq!(cli[0]["session"], "");
    assert_eq!(
        cli[0]["tokens"].as_u64(),
        Some(graphicle_memory::text::count(printed.trim_end()) as u64)
    );
    // `memories` returns the project's memory rows: counted, as the
    // `project_memory` tool that runs it; an operation that returns no
    // project text is not.
    let mut command = binary(&f);
    command.args(["call", "memories"]);
    let output = run_without_input(&mut command);
    assert!(output.status.success());
    let printed = String::from_utf8(output.stdout).unwrap();
    let cli = served("cli");
    assert_eq!(cli.len(), 2, "{cli:?}");
    let memories = cli.iter().find(|row| row["kind"] == "memories").unwrap();
    assert_eq!(
        memories["tokens"].as_u64(),
        Some(graphicle_memory::text::count(printed.trim_end()) as u64)
    );
    let mut command = binary(&f);
    command.args(["call", "build.info"]);
    assert!(run_without_input(&mut command).status.success());
    assert_eq!(served("cli").len(), 2);
}

#[test]
fn sessions_cli_imports_only_transcripts_of_synced_projects() {
    let f = Fixture::new();
    let folder = f.dir.path().join("transcripts/nested");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(
        folder.join("inside.jsonl"),
        claude(&f.root.to_string_lossy(), true),
    )
    .unwrap();
    let outside = claude("/somewhere/else", true).replace(CLAUDE_SESSION, "other-session");
    std::fs::write(folder.join("outside.jsonl"), outside).unwrap();
    std::fs::write(folder.join("broken.jsonl"), "{not a transcript").unwrap();
    let mut command = binary(&f);
    command
        .args(["sessions", "import", "--client", "claude-code", "--path"])
        .arg(f.dir.path().join("transcripts"));
    let output = run_without_input(&mut command);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["transcripts"], 3, "{report:#}");
    assert_eq!(report["sessions"], 1);
    assert_eq!(report["turns_imported"], 2);
    assert_eq!(report["skipped_outside_synced_projects"], 1);
    assert_eq!(report["skipped_unparsed"], 1);

    let mut command = binary(&f);
    command.args([
        "sessions",
        "forget",
        "--client",
        "claude-code",
        "--session-id",
        CLAUDE_SESSION,
    ]);
    let output = run_without_input(&mut command);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let forgotten: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(forgotten["forgotten_turns"], 2);
    assert!(f.stored("am_connector_docs").is_empty());
}

#[test]
fn connect_merges_claude_code_hooks_and_stays_idempotent() {
    let f = Fixture::new();
    let settings = f.root.join(".claude/settings.local.json");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::write(
        &settings,
        json!({"permissions":{"allow":["Bash(ls)"]},"hooks":{"Stop":[{"hooks":[{"type":"command","command":"echo other"}]}]}}).to_string(),
    )
    .unwrap();
    let connect = || {
        let mut command = binary(&f);
        command
            .args(["connect", "claude-code", "--hooks"])
            .current_dir(&f.root)
            .env("HOME", f.dir.path());
        let output = run_without_input(&mut command);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    let first = connect();
    assert_eq!(
        first["hooks"]["installed"].as_array().unwrap().len(),
        4,
        "{first:#}"
    );
    let document: Value = serde_json::from_slice(&std::fs::read(&settings).unwrap()).unwrap();
    assert_eq!(
        document["permissions"]["allow"][0], "Bash(ls)",
        "unrelated settings stay"
    );
    let stop = document["hooks"]["Stop"].as_array().unwrap();
    assert_eq!(stop.len(), 2, "{document:#}");
    assert_eq!(stop[0]["hooks"][0]["command"], "echo other");
    let ours = s(&stop[1]["hooks"][0], "command");
    // POSIX-quoted on macOS and Linux; on Windows the program comes first
    // with `/` separators, bare or double-quoted, so Git Bash and
    // PowerShell can run it.
    let program_first = if cfg!(windows) {
        let program = env!("CARGO_BIN_EXE_graphicle").replace('\\', "/");
        ours.starts_with(&format!("{program} ")) || ours.starts_with(&format!("\"{program}\" "))
    } else {
        ours.starts_with('\'')
    };
    assert!(
        ours.ends_with(" hook claude-code stop") && program_first,
        "{ours}"
    );
    assert!(f.root.join(".mcp.json").is_file());
    let start = &document["hooks"]["SessionStart"][0];
    assert_eq!(start["matcher"], "startup|clear|compact", "{document:#}");
    assert!(document["hooks"]["Stop"][1].get("matcher").is_none());

    let second = connect();
    assert!(
        second["hooks"]["installed"].as_array().unwrap().is_empty(),
        "{second:#}"
    );
    assert_eq!(second["hooks"]["unchanged"].as_array().unwrap().len(), 4);
    let again: Value = serde_json::from_slice(&std::fs::read(&settings).unwrap()).unwrap();
    assert_eq!(again, document);
}

#[test]
fn hooks_exit_zero_promptly_while_another_process_locks_the_store() {
    let f = Fixture::new();
    // macOS scans a freshly linked executable on its first launch; keep that
    // one-time cost out of the timings below.
    assert!(
        run_without_input(binary(&f).arg("--version"))
            .status
            .success()
    );
    // What one launch of the program costs on this machine now, timed like
    // the hooks. Each hook's bound is its budget plus LAUNCHES launches: a
    // hook run is a launch plus its own work (stop also starts a detached
    // import), and a machine slowed by other work slows both, so the bound
    // grows with the load instead of failing under it (stop once took 3.48 s
    // against a fixed 2 s while another cargo test shared the machine).
    const LAUNCHES: u32 = 4;
    let launch = {
        let mut command = binary(&f);
        command.arg("--version");
        let (output, took) = run_timed(command, "");
        assert!(output.status.success(), "{output:?}");
        took
    };
    // An exclusive-mode writer keeps every other connection, readers too,
    // out of the database until it closes.
    let lock = f.store.connect().unwrap();
    lock.pragma_update(None, "locking_mode", "EXCLUSIVE")
        .unwrap();
    lock.execute_batch(
        "BEGIN EXCLUSIVE; CREATE TABLE lock_probe(x); INSERT INTO lock_probe VALUES(1);",
    )
    .unwrap();
    let transcript = f.dir.path().join("locked.jsonl");
    std::fs::write(&transcript, claude(&f.root.to_string_lossy(), true)).unwrap();
    let cwd = f.root.to_string_lossy().into_owned();
    // Session start waits at most 2.5 s for context; stop only hands off.
    for (event, input, budget) in [
        (
            "session-start",
            json!({"cwd":cwd,"hook_event_name":"SessionStart","source":"startup"}),
            4,
        ),
        (
            "stop",
            json!({"cwd":cwd,"transcript_path":transcript,"hook_event_name":"Stop"}),
            2,
        ),
    ] {
        let mut command = binary(&f);
        command.args(["hook", "claude-code", event]);
        let (output, waited) = run_timed(command, &input.to_string());
        assert!(output.status.success(), "{event}: {output:?}");
        assert!(
            output.stdout.is_empty(),
            "{event} printed while the store was locked"
        );
        let bound = std::time::Duration::from_secs(budget) + launch * LAUNCHES;
        assert!(
            waited < bound,
            "{event} took {waited:?}, over {budget} s plus {LAUNCHES} launches of {launch:?}"
        );
    }
    lock.execute_batch("ROLLBACK").unwrap();
}
