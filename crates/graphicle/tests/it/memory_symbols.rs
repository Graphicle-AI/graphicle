//! Reviewed memory anchored to code symbols.
//!
//! A memory's quotes in code files resolve to the code symbols that contain
//! them, `propose_memory` and `note` name symbols and whole files, and
//! staleness follows the cited symbols instead of the whole file: the
//! staleness matrix, exact moves, quotes written before their code was projected
//! (pinned by the next projection), a PR scratch projection that never
//! moves the folder project's symbols, re-grounding on import,
//! certified forgetting of the link rows, the packet counters, and
//! conversation-only stores, which store and judge memories as before.
use graphicle_runtime::{
    jobs,
    store::{Store, s},
};
use serde_json::{Value, json};
use tempfile::TempDir;

const A: &str = "import os\n\n\ndef alpha():\n    return 1\n\n\ndef beta():\n    return 2\n";
const B: &str = "from a import alpha, beta\n\n\ndef run():\n    return alpha() + beta()\n";

struct Fixture {
    _dir: TempDir,
    store: Store,
    workspace: String,
    key: String,
    repo: String,
}

impl Fixture {
    /// A project holding `files`, synced; its projection only queued.
    fn queued(files: &[(&str, &str)]) -> Self {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let ws = store.create_workspace("Symbols").unwrap();
        let workspace = s(&ws, "id").to_string();
        let key = s(&ws, "capability").to_string();
        let mut f = Self {
            _dir: dir,
            store,
            workspace,
            key,
            repo: String::new(),
        };
        f.repo = f.project("symbols", "Symbols");
        f.sync(files);
        f
    }
    /// A project holding `files`, synced and projected.
    fn new(files: &[(&str, &str)]) -> Self {
        let f = Self::queued(files);
        f.drain();
        f
    }
    fn project(&self, key: &str, name: &str) -> String {
        let registered = self
            .store
            .register(
                &self.workspace,
                &self.key,
                &json!({"project_key":key,"name":name}),
            )
            .unwrap();
        s(&registered, "id").to_string()
    }
    fn args(&self, mut args: Value) -> Value {
        if args.get("repository_id").is_none() {
            args["repository_id"] = json!(self.repo);
        }
        args
    }
    fn sync(&self, files: &[(&str, &str)]) {
        self.sync_in(&self.repo, files);
    }
    fn sync_in(&self, repo: &str, files: &[(&str, &str)]) {
        let scoped = |mut args: Value| {
            args["repository_id"] = json!(repo);
            args
        };
        let status = self
            .store
            .project_status(&self.workspace, &self.key, &scoped(json!({})))
            .unwrap();
        let manifest: Vec<Value> = files
            .iter()
            .map(|(name, text)| json!({"path":name,"sha256":graphicle_memory::text::sha256(text.as_bytes()),"bytes":text.len()}))
            .collect();
        let begin = self
            .store
            .sync_prepare(
                &self.workspace,
                &self.key,
                &scoped(json!({"expected_snapshot":status["repository"]["snapshot"],"manifest":manifest})),
            )
            .unwrap();
        if begin["replayed"] == true {
            return;
        }
        let upload: Vec<Value> = files
            .iter()
            .map(|(name, text)| json!({"path":name,"content":text}))
            .collect();
        self.store
            .sync_upload(
                &self.workspace,
                &self.key,
                &scoped(json!({"sync_id":begin["sync_id"],"files":upload})),
            )
            .unwrap();
        self.store
            .sync_commit(
                &self.workspace,
                &self.key,
                &scoped(json!({"sync_id":begin["sync_id"]})),
            )
            .unwrap();
    }
    fn drain(&self) {
        self.drain_in(&self.repo);
    }
    fn drain_in(&self, repo: &str) {
        jobs::drain_projection(&self.store, repo).unwrap();
    }
    fn note(&self, quotes: &[(&str, &str)]) -> Value {
        self.note_with(quotes, json!({}))
    }
    fn note_with(&self, quotes: &[(&str, &str)], extra: Value) -> Value {
        let evidence: Vec<Value> = quotes
            .iter()
            .map(|(id, quote)| json!({"id":id,"quote":quote}))
            .collect();
        let mut args = json!({"title":"A decision","body":"Keep this as it is.","kind":"decision","evidence":evidence});
        for (key, value) in extra.as_object().unwrap() {
            args[key] = value.clone();
        }
        self.store
            .note(&self.workspace, &self.key, &self.args(args))
            .unwrap()
    }
    fn memories(&self) -> Vec<Value> {
        self.memories_in(&self.repo)
    }
    fn memories_in(&self, repo: &str) -> Vec<Value> {
        self.store
            .project_read(
                &self.workspace,
                &self.key,
                "memories",
                &json!({"repository_id":repo,"include_retired":true}),
            )
            .unwrap()
            .as_array()
            .unwrap()
            .clone()
    }
    fn memory(&self, id: &Value) -> Value {
        self.memories()
            .into_iter()
            .find(|m| m["id"] == *id)
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
    fn sql(&self, query: &str) -> Vec<Vec<Value>> {
        let db = self.store.connect().unwrap();
        let mut statement = db.prepare(query).unwrap();
        let columns = statement.column_count();
        statement
            .query_map([], |row| {
                (0..columns)
                    .map(|i| {
                        Ok(match row.get_ref(i)? {
                            rusqlite::types::ValueRef::Null => Value::Null,
                            rusqlite::types::ValueRef::Integer(n) => json!(n),
                            rusqlite::types::ValueRef::Text(t) => {
                                json!(String::from_utf8_lossy(t).into_owned())
                            }
                            other => json!(format!("{other:?}")),
                        })
                    })
                    .collect()
            })
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }
    /// The live symbol `qualified_name` of `relpath`: `(stable_id, revision)`.
    fn symbol_in(&self, repo: &str, relpath: &str, qualified: &str) -> (String, String) {
        let found = self.sql(&format!(
            "SELECT stable_id,revision FROM gf_symbol_heads WHERE repository_id='{repo}' AND relpath='{relpath}' AND qualified_name='{qualified}'"
        ));
        assert_eq!(found.len(), 1, "{relpath} {qualified}: {found:?}");
        (
            found[0][0].as_str().unwrap().to_string(),
            found[0][1].as_str().unwrap().to_string(),
        )
    }
    fn symbol(&self, relpath: &str, qualified: &str) -> (String, String) {
        self.symbol_in(&self.repo, relpath, qualified)
    }
    fn table_episode(&self, repo: &str) -> Value {
        self.sql(&format!(
            "SELECT episode FROM gf_symbol_state WHERE repository_id='{repo}'"
        ))
        .first()
        .map(|row| row[0].clone())
        .unwrap_or(Value::Null)
    }
    fn current_episode(&self, repo: &str) -> String {
        let db = self.store.connect().unwrap();
        graphicle_runtime::graph_projection::current_episode(&db, repo).unwrap()
    }
}

fn reasons(memory: &Value) -> Vec<String> {
    memory["stale_reasons"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r.as_str().unwrap().to_string())
        .collect()
}

#[test]
fn quotes_link_the_code_symbols_that_contain_them() {
    let f = Fixture::new(&[("a.py", A), ("notes.md", "# Notes\n\nKeep alpha small.\n")]);
    let (alpha, alpha_revision) = f.symbol("a.py", "alpha");
    let (beta, _) = f.symbol("a.py", "beta");
    let episode = f.current_episode(&f.repo);

    let one = f.note(&[("file:a.py", "    return 1")]);
    let data = &one["data"];
    let start = A.find("def alpha").unwrap();
    let end = A.find("def beta").unwrap();
    assert_eq!(
        data["symbols"],
        json!([{"state":"resolved","stable_id":alpha,"revision":alpha_revision,
            "identity":{"relpath":"a.py","qualified_name":"alpha","kind":"code","ordinal":0},
            "episode":episode,"byte_start":start,"byte_end":end,"rung":"quote","source":"file:a.py"}]),
        "{data:#}"
    );
    assert_eq!(data["symbol_files"], json!({"a.py":"symbols"}));
    assert_eq!(
        data["dependencies"]["a.py"],
        json!(graphicle_memory::text::sha256(A.as_bytes()))
    );

    // A quote across two symbols links both, and keeps the file rule.
    let both = f.note(&[("file:a.py", "return 1\n\n\ndef beta")]);
    let ids: Vec<&Value> = both["data"]["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| &l["stable_id"])
        .collect();
    assert_eq!(ids, [&json!(alpha), &json!(beta)]);
    assert_eq!(
        both["data"]["symbol_files"],
        json!({"a.py":"spans_symbols"})
    );

    // A quote in the module header, or in a file without code symbols,
    // stores what a memory stored before symbol links: the file rule.
    for quote in [
        ("file:a.py", "import os"),
        ("file:notes.md", "Keep alpha small."),
    ] {
        let memory = f.note(&[quote]);
        assert!(memory["data"].get("symbols").is_none(), "{memory:#}");
        assert!(memory["data"].get("symbol_files").is_none());
    }
    // Half in the header, half in alpha: linked, judged by the file rule.
    let half = f.note(&[("file:a.py", "os\n\n\ndef alpha")]);
    assert_eq!(
        half["data"]["symbol_files"],
        json!({"a.py":"outside_symbols"})
    );

    // The listing reports the links of a memory that has them, only.
    let listed = f.memory(&one["id"]);
    assert_eq!(
        listed["symbol_links"],
        json!({"links":1,"followed_move":0,"pending":0})
    );
    assert!(f.memories().iter().any(|m| m.get("symbol_links").is_none()));
}

/// The staleness matrix: after a memory quotes `quote` in `before`, `after`
/// is synced and projected.
#[test]
fn staleness_follows_the_cited_symbols_not_the_file() {
    const CLASS_BEFORE: &str =
        "class C:\n    def m1(self):\n        return 1\n\n    def m3(self):\n        return 3\n";
    const CLASS_AFTER: &str = "class C:\n    def m1(self):\n        return 1\n\n    def m2(self):\n        return 2\n\n    def m3(self):\n        return 3\n";
    let edited_elsewhere = A.replace("return 2", "return 20");
    let inserted_above = A.replace("import os\n", "import os\nimport sys\n\nLIMIT = 3\n");
    let edited_inside = A.replace("return 1", "return 10");
    enum Expect {
        Served,
        Withheld(&'static str),
    }
    /// (name, files before, quote, files after, expected)
    type Case<'a> = (&'a str, &'a str, &'a str, Vec<(&'a str, String)>, Expect);
    let cases: Vec<Case> = vec![
        (
            "edit elsewhere in the same file",
            A,
            "    return 1",
            vec![("a.py", edited_elsewhere.clone())],
            Expect::Served,
        ),
        (
            "lines inserted above the symbol",
            A,
            "    return 1",
            vec![("a.py", inserted_above)],
            Expect::Served,
        ),
        (
            "edit inside the symbol",
            A,
            "    return 1",
            vec![("a.py", edited_inside)],
            Expect::Withheld("symbol_changed:"),
        ),
        (
            "method added to a class",
            CLASS_BEFORE,
            "        return 1",
            vec![("a.py", CLASS_AFTER.to_string())],
            Expect::Served,
        ),
        (
            "quote in the module header",
            A,
            "import os",
            vec![("a.py", edited_elsewhere.clone())],
            Expect::Withheld("source_changed_or_unavailable:file:a.py"),
        ),
        // The engine keeps one node per label in a file (identity.rs, parity),
        // so a same-name definition inserted above changes that node's bytes.
        (
            "same-name definition inserted above",
            "def f():\n    return 1\n",
            "    return 1",
            vec![(
                "a.py",
                "def f():\n    return 0\n\n\ndef f():\n    return 1\n".to_string(),
            )],
            Expect::Withheld("symbol_"),
        ),
        (
            "unchanged file moved",
            A,
            "    return 1",
            vec![("pkg/moved.py", A.to_string())],
            Expect::Served,
        ),
    ];
    for (name, before, quote, after, expect) in cases {
        let f = Fixture::new(&[("a.py", before)]);
        let memory = f.note(&[("file:a.py", quote)]);
        let after: Vec<(&str, &str)> = after.iter().map(|(p, t)| (*p, t.as_str())).collect();
        f.sync(&after);
        f.drain();
        let listed = f.memory(&memory["id"]);
        match expect {
            Expect::Served => assert_eq!(listed["stale"], false, "{name}: {listed:#}"),
            Expect::Withheld(prefix) => {
                assert_eq!(listed["stale"], true, "{name}: {listed:#}");
                assert!(
                    reasons(&listed).iter().any(|r| r.starts_with(prefix)),
                    "{name}: {:?}",
                    reasons(&listed)
                );
            }
        }
        if name == "edit inside the symbol" {
            let (alpha, _) = f.symbol("a.py", "alpha");
            assert_eq!(reasons(&listed), [format!("symbol_changed:{alpha}")]);
        }
        if name == "unchanged file moved" {
            assert_eq!(listed["symbol_links"]["followed_move"], 1, "{listed:#}");
            let (moved, _) = f.symbol("pkg/moved.py", "alpha");
            let at = f.sql("SELECT from_stable_id,to_stable_id FROM gf_symbol_moves");
            assert!(at.iter().any(|row| row[1] == json!(moved)), "{at:?}");
            // The quote is found again where the symbol is now.
            assert_eq!(
                listed["symbol_links"]["locations"]["file:a.py"][0]["id"],
                "file:pkg/moved.py"
            );
        }
    }

    // The projection is still queued: withheld until it runs.
    let f = Fixture::new(&[("a.py", A)]);
    let memory = f.note(&[("file:a.py", "    return 1")]);
    let (alpha, _) = f.symbol("a.py", "alpha");
    f.sync(&[("a.py", &edited_elsewhere)]);
    let waiting = f.memory(&memory["id"]);
    assert_eq!(waiting["stale"], true);
    assert_eq!(reasons(&waiting), [format!("projection_pending:{alpha}")]);
    assert_eq!(waiting["symbol_links"]["pending"], 1);
    f.drain();
    assert_eq!(f.memory(&memory["id"])["stale"], false);
}

/// A quote across several code symbols. Their revisions say nothing
/// about whether they are still adjacent: a symbol inserted between them,
/// the two reordered, or each moved to a file of its own leaves both
/// revisions unchanged while the quoted text is gone. Such a quote records
/// both links and keeps the file rule (`spans_symbols`).
#[test]
fn a_quote_across_symbols_keeps_the_file_rule() {
    const THREE: &str = "def alpha():\n    return 1\n\n\ndef beta():\n    return 2\n\n\ndef gamma():\n    return 3\n";
    const QUOTE: &str = "return 1\n\n\ndef beta";
    /// (name, files after, where alpha and beta are then)
    type Case<'a> = (&'a str, Vec<(&'a str, String)>, (&'a str, &'a str));
    let cases: Vec<Case> = vec![
        (
            "a symbol inserted between them",
            vec![(
                "a.py",
                THREE.replace("def beta", "def delta():\n    return 4\n\n\ndef beta"),
            )],
            ("a.py", "a.py"),
        ),
        (
            "the two reordered",
            vec![(
                "a.py",
                "def beta():\n    return 2\n\n\ndef alpha():\n    return 1\n\n\ndef gamma():\n    return 3\n"
                    .to_string(),
            )],
            ("a.py", "a.py"),
        ),
        (
            "each moved to a file of its own",
            vec![
                ("x.py", "def alpha():\n    return 1\n\n\n".to_string()),
                ("y.py", "def beta():\n    return 2\n\n\n".to_string()),
                ("a.py", "def gamma():\n    return 3\n".to_string()),
            ],
            ("x.py", "y.py"),
        ),
    ];
    for (name, after, (alpha_at, beta_at)) in cases {
        let f = Fixture::new(&[("a.py", THREE)]);
        let (alpha, alpha_revision) = f.symbol("a.py", "alpha");
        let (beta, beta_revision) = f.symbol("a.py", "beta");
        let memory = f.note(&[("file:a.py", QUOTE)]);
        let linked: Vec<&Value> = memory["data"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| &l["stable_id"])
            .collect();
        assert_eq!(linked, [&json!(alpha), &json!(beta)], "{name}");
        let files: Vec<(&str, &str)> = after.iter().map(|(p, t)| (*p, t.as_str())).collect();
        f.sync(&files);
        f.drain();
        // Both symbols keep their bytes; the quote is nowhere.
        assert_eq!(f.symbol(alpha_at, "alpha").1, alpha_revision, "{name}");
        assert_eq!(f.symbol(beta_at, "beta").1, beta_revision, "{name}");
        assert!(
            files.iter().all(|(_, text)| !text.contains(QUOTE)),
            "{name}"
        );
        let listed = f.memory(&memory["id"]);
        assert_eq!(listed["stale"], true, "{name}: {listed:#}");
        assert_eq!(
            reasons(&listed),
            ["source_changed_or_unavailable:file:a.py"],
            "{name}"
        );
        assert_eq!(
            memory["data"]["symbol_files"],
            json!({"a.py":"spans_symbols"}),
            "{name}"
        );
    }

    // Written before the projection: the pin finds the two symbols and keeps
    // the file rule for that quote.
    let f = Fixture::queued(&[("a.py", THREE)]);
    let memory = f.note(&[("file:a.py", QUOTE)]);
    assert_eq!(memory["data"]["symbol_files"], json!({"a.py":"pending"}));
    f.drain();
    let (alpha, _) = f.symbol("a.py", "alpha");
    let (beta, _) = f.symbol("a.py", "beta");
    let pins = f.sql("SELECT state,covered,symbols FROM gf_symbol_pins");
    assert_eq!((&pins[0][0], &pins[0][1]), (&json!("pinned"), &json!(0)));
    let pinned: Vec<Value> = serde_json::from_str(pins[0][2].as_str().unwrap()).unwrap();
    let pinned: Vec<&Value> = pinned.iter().map(|l| &l["stable_id"]).collect();
    assert_eq!(pinned, [&json!(alpha), &json!(beta)]);
    f.sync(&[(
        "a.py",
        &THREE.replace("def beta", "def delta():\n    return 4\n\n\ndef beta"),
    )]);
    f.drain();
    assert_eq!(
        reasons(&f.memory(&memory["id"])),
        ["source_changed_or_unavailable:file:a.py"]
    );
}

/// A retained answer links only the windows of code symbols the
/// live-symbol table holds at the window's revision. A window on the file
/// node, on a rationale comment, or on a node the table does not hold at
/// that revision is not linked, so an edit elsewhere in the file never reads
/// it as `symbol_missing` (and `by_symbol` never counts it).
#[test]
fn a_retained_answer_links_only_windows_of_live_code_symbols() {
    const NOTED: &str = "import os\n\n\ndef alpha():\n    return 1\n\n\n# NOTE: beta stays pure.\ndef beta():\n    return 2\n";
    let f = Fixture::new(&[("a.py", NOTED)]);
    let (alpha, alpha_revision) = f.symbol("a.py", "alpha");
    let episode = f.current_episode(&f.repo);
    let db = f.store.connect().unwrap();
    let graph = graphicle_runtime::graph_projection::resident(&db, &f.repo, &episode).unwrap();
    // The code-graph facts the arm reports for the window of the node of
    // a.py whose `key` is `value`.
    let facts = |key: &str, value: &str| -> Value {
        let node = graph
            .nodes()
            .iter()
            .find(|n| n.attributes[key] == value && n.attributes["relpath"] == "a.py")
            .unwrap_or_else(|| panic!("no node with {key} {value}"));
        let attributes = &node.attributes;
        json!({"kind":attributes["kind"],"stable_id":attributes["stable_id"],
            "revision":attributes["revision"],"relpath":"a.py",
            "qualified_name":attributes["qualified_name"],"episode":episode,
            "byte_start":0,"byte_end":NOTED.len()})
    };
    let file_node = facts("kind", "file");
    let rationale = facts("kind", "rationale");
    let mut alpha_window = facts("qualified_name", "alpha");
    assert_eq!(alpha_window["stable_id"], json!(alpha), "{alpha_window:#}");
    for other in [&file_node, &rationale] {
        assert!(other["stable_id"].as_str().is_some_and(|id| id != alpha));
        assert!(other["revision"].as_str().is_some(), "{other:#}");
    }
    let mut changed = alpha_window.clone();
    changed["revision"] = json!("an older revision");
    alpha_window["byte_start"] = json!(NOTED.find("def alpha").unwrap());
    alpha_window["byte_end"] = json!(NOTED.find("# NOTE").unwrap());
    // Every window holds the answer's quote; only alpha's is a live symbol.
    let windows: Vec<Value> = [&file_node, &rationale, &changed, &alpha_window]
        .iter()
        .map(|facts| {
            json!({"id":"file:a.py","byte_start":facts["byte_start"],"byte_end":facts["byte_end"],
                "code_graph":facts})
        })
        .collect();
    let citations = [json!({"id":"file:a.py","quote":"    return 1"})];
    let documents = [
        json!({"id":"file:a.py","path":"a.py","source_kind":"repository_file",
        "sha256":graphicle_memory::text::sha256(NOTED.as_bytes()),"content":NOTED}),
    ];
    let mut data =
        graphicle_runtime::memory::dependencies(&db, &f.repo, &citations, &documents).unwrap();
    graphicle_runtime::symbols::carry_windows(
        &db, &f.repo, &citations, &windows, &documents, &mut data,
    )
    .unwrap();
    let mut links: Vec<(String, String, String)> = data["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            (
                s(l, "stable_id").to_string(),
                s(l, "revision").to_string(),
                s(l, "rung").to_string(),
            )
        })
        .collect();
    links.sort();
    assert_eq!(
        links,
        [
            (
                alpha.clone(),
                alpha_revision.clone(),
                "answer_window".into()
            ),
            (alpha.clone(), alpha_revision, "quote".into())
        ]
    );
    let window = data["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["rung"] == "answer_window")
        .unwrap();
    assert_eq!(
        window["identity"],
        json!({"relpath":"a.py","qualified_name":"alpha","kind":"code","ordinal":0})
    );
    assert_eq!(data["symbol_files"], json!({"a.py":"symbols"}));
    // Retained, it stays current through an edit of beta.
    let saved = graphicle_runtime::memory::save(
        &db,
        &f.repo,
        None,
        None,
        "note",
        "Alpha returns one",
        "alpha() returns 1.",
        "reviewed",
        &data,
    )
    .unwrap();
    f.sync(&[("a.py", &NOTED.replace("return 2", "return 20"))]);
    f.drain();
    let listed = f.memory(&saved["id"]);
    assert_eq!(listed["stale"], false, "{listed:#}");
    assert_eq!(listed["symbol_links"]["links"], 1);
}

/// An id that now names another symbol while the cited bytes live on under
/// the same name at another ordinal reads as missing, not changed. The
/// engine collapses same-label symbols of a file, so the shift is written
/// into the table directly here.
#[test]
fn an_ordinal_shift_reads_as_a_missing_symbol() {
    let f = Fixture::new(&[("a.py", A)]);
    let memory = f.note(&[("file:a.py", "    return 1")]);
    let (alpha, revision) = f.symbol("a.py", "alpha");
    let edited = A.replace("return 2", "return 20");
    f.sync(&[("a.py", &edited)]);
    f.drain();
    assert_eq!(f.memory(&memory["id"])["stale"], false);
    let db = f.store.connect().unwrap();
    db.execute(
        "UPDATE gf_symbol_heads SET revision='another' WHERE stable_id=?",
        [&alpha],
    )
    .unwrap();
    let changed = f.memory(&memory["id"]);
    assert_eq!(reasons(&changed), [format!("symbol_changed:{alpha}")]);
    db.execute(
        "INSERT INTO gf_symbol_heads(repository_id,stable_id,revision,relpath,qualified_name,kind,ordinal,node_id,line_start,line_next,sha256) SELECT repository_id,'shifted',?,relpath,qualified_name,kind,1,'a_alpha_1',line_start,line_next,sha256 FROM gf_symbol_heads WHERE stable_id=?",
        [&revision, &alpha],
    )
    .unwrap();
    let missing = f.memory(&memory["id"]);
    assert_eq!(reasons(&missing), [format!("symbol_missing:{alpha}")]);
}

#[test]
fn quotes_written_before_the_projection_are_pinned_by_it() {
    let f = Fixture::queued(&[
        ("a.py", A),
        ("session-1.txt", "user: keep alpha returning one\n"),
        (
            "session-2.json",
            "{\"turns\": [{\"user\": \"keep beta returning two\"}]}\n",
        ),
        ("pyproject.toml", "[project]\nname = \"symbols\"\n"),
    ]);
    // No episode yet: the code quote waits; the session quote is as before,
    // and so are data and a package manifest, which the engine
    // classifies as code.
    for quote in [
        ("file:session-2.json", "keep beta returning two"),
        ("file:pyproject.toml", "name = \"symbols\""),
    ] {
        let data = &f.note(&[quote])["data"];
        assert!(data.get("symbols").is_none(), "{quote:?}: {data:#}");
        assert!(data.get("symbol_files").is_none());
    }
    let memory = f.note(&[("file:a.py", "    return 1")]);
    let start = A.find("    return 1").unwrap();
    assert_eq!(
        memory["data"]["symbols"],
        json!([{"state":"pending","relpath":"a.py","sha256":graphicle_memory::text::sha256(A.as_bytes()),
            "byte_start":start,"byte_end":start + 12,"rung":"quote","source":"file:a.py"}])
    );
    assert_eq!(memory["data"]["symbol_files"], json!({"a.py":"pending"}));
    let session = f.note(&[("file:session-1.txt", "keep alpha returning one")]);
    assert!(session["data"].get("symbols").is_none());
    let listed = f.memory(&memory["id"]);
    assert_eq!(listed["stale"], false);
    assert_eq!(
        listed["symbol_links"],
        json!({"links":0,"followed_move":0,"pending":1})
    );

    // The projection pins it to alpha.
    f.drain();
    let (alpha, revision) = f.symbol("a.py", "alpha");
    let pins = f.sql("SELECT state,covered,symbols FROM gf_symbol_pins");
    assert_eq!(pins.len(), 1);
    assert_eq!((&pins[0][0], &pins[0][1]), (&json!("pinned"), &json!(1)));
    let pinned: Value = serde_json::from_str(pins[0][2].as_str().unwrap()).unwrap();
    assert_eq!(pinned[0]["stable_id"], json!(alpha));
    assert_eq!(pinned[0]["revision"], json!(revision));
    assert_eq!(
        f.memory(&memory["id"])["symbol_links"],
        json!({"links":1,"followed_move":0,"pending":0})
    );

    // From then on it follows alpha.
    f.sync(&[("a.py", &A.replace("return 2", "return 20"))]);
    f.drain();
    assert_eq!(f.memory(&memory["id"])["stale"], false);
    f.sync(&[("a.py", &A.replace("return 1", "return 10"))]);
    f.drain();
    let changed = f.memory(&memory["id"]);
    assert_eq!(reasons(&changed), [format!("symbol_changed:{alpha}")]);

    // A quote whose content changed before any projection read it can never
    // be pinned: the file rule.
    let g = Fixture::queued(&[("a.py", A)]);
    let early = g.note(&[("file:a.py", "    return 1")]);
    g.sync(&[("a.py", &A.replace("return 2", "return 20"))]);
    g.drain();
    let unpinned = g.memory(&early["id"]);
    assert_eq!(
        reasons(&unpinned),
        ["source_changed_or_unavailable:file:a.py"]
    );
}

#[test]
fn propose_memory_names_files_and_symbols() {
    let f = Fixture::new(&[("a.py", A), ("b.py", B)]);
    let propose = |extra: Value, key: &str| {
        let mut args = json!({"kind":"decision","title":"Run adds both","body":"run() adds alpha and beta.",
            "evidence":[{"source_id":"file:b.py","quote":"    return alpha() + beta()"}],"idempotency_key":key});
        for (field, value) in extra.as_object().unwrap() {
            args[field] = value.clone();
        }
        f.store
            .project_mutate(&f.workspace, &f.key, "memory.propose", &f.args(args))
    };
    let unknown = propose(json!({"files":["missing.py"]}), "k1").unwrap_err();
    assert!(
        unknown
            .message
            .contains("missing.py is not in the project's current sources"),
        "{}",
        unknown.message
    );
    let nameless = propose(json!({"symbols":[{"stable_id":"nope"}]}), "k2").unwrap_err();
    assert!(
        nameless.message.contains("No code symbol nope"),
        "{}",
        nameless.message
    );
    let (alpha, _) = f.symbol("a.py", "alpha");
    let (run, _) = f.symbol("b.py", "run");
    let saved = propose(
        json!({"files":["b.py"],"symbols":[{"path":"a.py","qualified_name":"alpha"}]}),
        "k3",
    )
    .unwrap();
    // Replays compare the whole proposal, symbols included.
    assert_eq!(
        propose(
            json!({"files":["b.py"],"symbols":[{"path":"a.py","qualified_name":"alpha"}]}),
            "k3"
        )
        .unwrap()["replayed"],
        true
    );
    let draft = f.memory(&saved["memory_id"]);
    let data = &draft["data"];
    assert_eq!(
        data["symbol_files"],
        json!({"a.py":"symbols","b.py":"named_file"})
    );
    let mut linked: Vec<(String, String)> = data["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| (s(l, "stable_id").to_string(), s(l, "rung").to_string()))
        .collect();
    linked.sort();
    let mut expected = vec![
        (alpha.clone(), "named".to_string()),
        (run, "quote".to_string()),
    ];
    expected.sort();
    assert_eq!(linked, expected);
    assert_eq!(
        data["dependencies"],
        json!({"a.py":graphicle_memory::text::sha256(A.as_bytes()),"b.py":graphicle_memory::text::sha256(B.as_bytes())})
    );
    f.store
        .review_memory(
            &f.workspace,
            &f.key,
            &f.args(json!({"memory_id":draft["id"],"expected_revision":draft["revision"],"decision":"approve","maintainer":"Reviewer","reason":"Checked"})),
        )
        .unwrap();
    // An edit elsewhere in a.py leaves alpha's memory current.
    f.sync(&[("a.py", &A.replace("return 2", "return 20")), ("b.py", B)]);
    f.drain();
    assert_eq!(f.memory(&saved["memory_id"])["stale"], false);
    // b.py was named as a whole: any edit withholds it.
    f.sync(&[
        ("a.py", &A.replace("return 2", "return 20")),
        ("b.py", &format!("{B}\n# tail\n")),
    ]);
    f.drain();
    let named = f.memory(&saved["memory_id"]);
    assert_eq!(reasons(&named), ["source_changed_or_unavailable:file:b.py"]);

    // Ambiguity and a code graph that lags behind the file are refused.
    let g = Fixture::new(&[("a.py", A)]);
    let db = g.store.connect().unwrap();
    db.execute(
        "INSERT INTO gf_symbol_heads(repository_id,stable_id,revision,relpath,qualified_name,kind,ordinal,node_id,line_start,line_next,sha256) SELECT repository_id,'twin',revision,relpath,qualified_name,'other',0,'twin',line_start,line_next,sha256 FROM gf_symbol_heads WHERE qualified_name='alpha'",
        [],
    )
    .unwrap();
    let note = |symbols: Value| {
        g.store.note(
            &g.workspace,
            &g.key,
            &g.args(json!({"title":"t","body":"b","symbols":symbols})),
        )
    };
    let ambiguous = note(json!([{"path":"a.py","qualified_name":"alpha"}])).unwrap_err();
    assert!(
        ambiguous.message.contains("2 code symbols match"),
        "{}",
        ambiguous.message
    );
    let chosen = note(json!([{"path":"a.py","qualified_name":"alpha","kind":"code"}])).unwrap();
    assert_eq!(chosen["data"]["symbol_files"], json!({"a.py":"symbols"}));
    g.sync(&[("a.py", &A.replace("return 1", "return 10"))]);
    let lagging =
        note(json!([{"path":"a.py","qualified_name":"alpha","kind":"code"}])).unwrap_err();
    assert!(
        lagging
            .message
            .contains("not built for its current content"),
        "{}",
        lagging.message
    );
}

#[test]
fn import_regrounds_links_through_the_logical_identity() {
    let f = Fixture::new(&[("a.py", A)]);
    let memory = f.note(&[("file:a.py", "    return 1")]);
    let exported = f
        .store
        .project_read(&f.workspace, &f.key, "memory.export", &f.args(json!({})))
        .unwrap();
    let export: Vec<Value> = exported["memories"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["id"] == memory["id"])
        .cloned()
        .collect();

    // The same code in another project: other stable ids, same identity.
    let same = f.project("same", "Same");
    f.sync_in(&same, &[("a.py", A)]);
    f.drain_in(&same);
    let (theirs, revision) = f.symbol_in(&same, "a.py", "alpha");
    let (ours, _) = f.symbol("a.py", "alpha");
    assert_ne!(theirs, ours);
    let imported = f
        .store
        .project_mutate(
            &f.workspace,
            &f.key,
            "memory.import",
            &json!({"repository_id":same,"memories":export}),
        )
        .unwrap();
    assert_eq!(
        imported["symbol_links"],
        json!({"regrounded":1,"unresolved":0})
    );
    let link = &imported["imported"][0]["data"]["symbols"][0];
    assert_eq!(
        (&link["state"], &link["stable_id"], &link["revision"]),
        (&json!("resolved"), &json!(theirs), &json!(revision))
    );
    assert_eq!(
        imported["imported"][0]["data"]["symbol_files"],
        json!({"a.py":"symbols"})
    );
    let draft = f
        .memories_in(&same)
        .into_iter()
        .find(|m| m["id"] == imported["imported"][0]["id"])
        .unwrap();
    assert_eq!(draft["stale"], false);

    // Imported links are data: links that do not cover the memory's own
    // quotes leave its file to the file rule.
    let mut tampered = export.clone();
    tampered[0]["data"]["evidence"] = json!([{"id":"file:a.py","quote":"    return 2"}]);
    let imported = f
        .store
        .project_mutate(
            &f.workspace,
            &f.key,
            "memory.import",
            &json!({"repository_id":same,"memories":tampered}),
        )
        .unwrap();
    assert_eq!(
        imported["imported"][0]["data"]["symbol_files"],
        json!({"a.py":"unresolved"})
    );

    // Code whose alpha differs: the link cannot be re-grounded.
    let other = f.project("other", "Other");
    f.sync_in(&other, &[("a.py", &A.replace("return 1", "return 10"))]);
    f.drain_in(&other);
    let imported = f
        .store
        .project_mutate(
            &f.workspace,
            &f.key,
            "memory.import",
            &json!({"repository_id":other,"memories":export}),
        )
        .unwrap();
    assert_eq!(
        imported["symbol_links"],
        json!({"regrounded":0,"unresolved":1})
    );
    let data = &imported["imported"][0]["data"];
    assert_eq!(
        (&data["symbols"][0]["state"], &data["symbols"][0]["reason"]),
        (&json!("unresolved"), &json!("revision_differs"))
    );
    assert_eq!(data["symbol_files"], json!({"a.py":"unresolved"}));
    let draft = f
        .memories_in(&other)
        .into_iter()
        .find(|m| m["id"] == imported["imported"][0]["id"])
        .unwrap();
    assert_eq!(draft["stale"], true);
}

/// An imported file of pending quotes keeps them only when every
/// occurrence of the memory's quotes there has a pending entry, and more
/// than 200 links are dropped unread.
#[test]
fn import_keeps_pending_quotes_only_when_each_occurrence_has_one() {
    let f = Fixture::queued(&[("a.py", A)]);
    let memory = f.note(&[("file:a.py", "    return 1"), ("file:a.py", "    return 2")]);
    assert_eq!(memory["data"]["symbol_files"], json!({"a.py":"pending"}));
    assert_eq!(memory["data"]["symbols"].as_array().unwrap().len(), 2);
    let exported = f
        .store
        .project_read(&f.workspace, &f.key, "memory.export", &f.args(json!({})))
        .unwrap();
    let export: Vec<Value> = exported["memories"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["id"] == memory["id"])
        .cloned()
        .collect();
    let target = f.project("target", "Target");
    f.sync_in(&target, &[("a.py", A)]);
    let import = |memories: Vec<Value>| {
        f.store
            .project_mutate(
                &f.workspace,
                &f.key,
                "memory.import",
                &json!({"repository_id":target,"memories":memories}),
            )
            .unwrap()
    };
    let whole = import(export.clone());
    assert_eq!(
        whole["imported"][0]["data"]["symbol_files"],
        json!({"a.py":"pending"})
    );
    // An export that kept the pending entry of one quote only.
    let mut partial = export.clone();
    partial[0]["data"]["symbols"] = json!([export[0]["data"]["symbols"][0]]);
    let partial = import(partial);
    assert_eq!(
        partial["imported"][0]["data"]["symbol_files"],
        json!({"a.py":"unresolved"})
    );
    // Past 200 links nothing is read or kept.
    let mut many = export.clone();
    many[0]["data"]["symbols"] = json!(vec![export[0]["data"]["symbols"][0].clone(); 201]);
    let many = import(many);
    assert_eq!(
        many["symbol_links"],
        json!({"regrounded":0,"unresolved":201})
    );
    let data = &many["imported"][0]["data"];
    assert!(data.get("symbols").is_none() && data.get("symbol_files").is_none());

    // Projected there, lines inserted above alpha and beta: the draft whose
    // pending quotes were all kept is judged by their pins, the others by
    // the file rule.
    f.drain_in(&target);
    f.sync_in(
        &target,
        &[("a.py", &A.replace("import os\n", "import os\nimport sys\n"))],
    );
    f.drain_in(&target);
    let stale = |imported: &Value| {
        f.memories_in(&target)
            .into_iter()
            .find(|m| m["id"] == imported["imported"][0]["id"])
            .unwrap()["stale"]
            .clone()
    };
    assert_eq!(stale(&whole), false);
    assert_eq!(stale(&partial), true);
    assert_eq!(stale(&many), true);
}

#[test]
fn deleting_or_forgetting_a_memory_takes_its_symbol_pins() {
    let f = Fixture::queued(&[("a.py", A)]);
    let connector = f
        .store
        .connector_add(
            &f.workspace,
            &f.key,
            &f.args(json!({"kind":"jsonl","name":"Decisions","config":{"acl":{"workspace_readers":true,"credentials":[]}}})),
        )
        .unwrap();
    let connector = s(&connector, "id").to_string();
    f.store
        .import_documents(
            &f.workspace,
            &f.key,
            &f.args(json!({"connector_id":connector,"documents":[{"id":"d1","title":"Decision","content":"Alpha must return one."}]})),
        )
        .unwrap();
    let cited = f.note(&[
        (
            &format!("connector:{connector}:d1"),
            "Alpha must return one.",
        ),
        ("file:a.py", "    return 1"),
    ]);
    let alone = f.note(&[("file:a.py", "    return 2")]);
    assert_eq!(f.sql("SELECT state FROM gf_symbol_pins").len(), 2);

    let deleted = f
        .store
        .project_mutate(
            &f.workspace,
            &f.key,
            "memory.delete",
            &f.args(json!({"memory_id":alone["id"]})),
        )
        .unwrap();
    let receipt = &deleted["receipt"];
    assert_eq!(
        receipt["removed"]["rows"]["gf_symbol_pins"], 1,
        "{receipt:#}"
    );
    assert_eq!(f.sql("SELECT state FROM gf_symbol_pins").len(), 1);

    let forgot = f
        .store
        .maintain(
            &f.workspace,
            &f.key,
            "source.forget",
            &f.args(json!({"connector_id":connector,"document_ids":["d1"]})),
        )
        .unwrap();
    let receipt = &forgot["receipt"];
    assert_eq!(receipt["verified_local"], true, "{receipt:#}");
    assert_eq!(
        receipt["removed"]["derived"]["symbol_pins"], 1,
        "{receipt:#}"
    );
    assert!(f.memories().iter().all(|m| m["id"] != cited["id"]));
    assert!(f.sql("SELECT state FROM gf_symbol_pins").is_empty());
}

#[test]
fn context_counts_symbol_withholding_moves_and_pending_links() {
    let f = Fixture::new(&[("a.py", A), ("b.py", B)]);
    let alpha = f.note_with(
        &[("file:a.py", "    return 1")],
        json!({"title":"Alpha stays one","body":"alpha returns one"}),
    );
    let run = f.note_with(
        &[("file:b.py", "    return alpha() + beta()")],
        json!({"title":"Run sums","body":"run sums alpha and beta"}),
    );
    // alpha changes; b.py moves unchanged.
    f.sync(&[
        ("a.py", &A.replace("return 1", "return 10")),
        ("lib/b.py", B),
    ]);
    f.drain();
    let packet = f.context("run sums alpha and beta");
    let mechanisms = &packet["mechanisms"];
    assert_eq!(
        mechanisms["memories_withheld"],
        json!({"by_file":0,"by_symbol":1}),
        "{mechanisms:#}"
    );
    assert_eq!(mechanisms["links_followed_through_move"], 1);
    assert_eq!(mechanisms["links_pending_projection"], 0);
    assert_eq!(f.memory(&alpha["id"])["stale"], true);
    // The moved memory's quote is found again in the moved file.
    let support = packet["memory_support"]["memories"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == run["id"])
        .cloned()
        .unwrap_or_else(|| panic!("{:#}", packet["memory_support"]));
    assert_eq!(
        (
            &support["located_citations"],
            &support["unavailable_citations"]
        ),
        (&json!(1), &json!(0)),
        "{support:#}"
    );
    // A sync waiting for its projection reports the links it holds back.
    f.sync(&[
        ("a.py", &A.replace("return 1", "return 10")),
        ("lib/b.py", &format!("{B}\n\ndef other():\n    return 3\n")),
    ]);
    let packet = f.context("run sums alpha and beta");
    assert_eq!(packet["mechanisms"]["links_pending_projection"], 1);
}

/// Conversation stores stay as they were: a conversation-only store's
/// memories store and are judged exactly as before.
#[test]
fn a_conversation_project_keeps_its_memories_as_before() {
    let session = "user: I adopted a cat named Luna.\n";
    let notes = "# Notes\n\nLuna sees the vet on Elm Street.\n";
    for projected in [false, true] {
        let f = if projected {
            Fixture::new(&[("session-1.txt", session), ("notes.md", notes)])
        } else {
            Fixture::queued(&[("session-1.txt", session), ("notes.md", notes)])
        };
        let memory = f.note(&[
            ("file:session-1.txt", "I adopted a cat named Luna."),
            ("file:notes.md", "Luna sees the vet on Elm Street."),
        ]);
        let data = &memory["data"];
        assert!(data.get("symbols").is_none() && data.get("symbol_files").is_none());
        let mut keys: Vec<&String> = data.as_object().unwrap().keys().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "authority",
                "dependencies",
                "evidence",
                "evidence_links",
                "maintainer",
                "native_sources",
                "notes"
            ]
        );
        assert!(f.sql("SELECT * FROM gf_symbol_heads").is_empty());
        assert!(f.sql("SELECT * FROM gf_symbol_pins").is_empty());
        f.sync(&[
            (
                "session-1.txt",
                "user: I adopted a cat named Luna. She is two.\n",
            ),
            ("notes.md", notes),
        ]);
        f.drain();
        let listed = f.memory(&memory["id"]);
        assert_eq!(
            reasons(&listed),
            ["source_changed_or_unavailable:file:session-1.txt"]
        );
        assert!(listed.get("symbol_links").is_none());
        let mechanisms = &f.context("Where does Luna see the vet?")["mechanisms"];
        assert_eq!(
            (
                &mechanisms["memories_withheld"],
                &mechanisms["links_followed_through_move"],
                &mechanisms["links_pending_projection"]
            ),
            (&json!({"by_file":1,"by_symbol":0}), &json!(0), &json!(0))
        );
    }
}

#[test]
fn the_live_symbol_table_follows_the_projected_head_set() {
    let f = Fixture::new(&[("a.py", A)]);
    let first = f.current_episode(&f.repo);
    assert_eq!(f.table_episode(&f.repo), json!(first));
    let rows = f.sql(
        "SELECT relpath,qualified_name,kind,ordinal,line_start,line_next FROM gf_symbol_heads ORDER BY line_start",
    );
    assert_eq!(
        rows,
        vec![
            vec![
                json!("a.py"),
                json!("alpha"),
                json!("code"),
                json!(0),
                json!(4),
                json!(8)
            ],
            vec![
                json!("a.py"),
                json!("beta"),
                json!("code"),
                json!(0),
                json!(8),
                Value::Null
            ],
        ]
    );
    let alpha_before = f.symbol("a.py", "alpha");
    // A new file: a.py's rows stay as they were, b.py's join.
    f.sync(&[("a.py", A), ("b.py", B)]);
    f.drain();
    let second = f.current_episode(&f.repo);
    assert_eq!(f.table_episode(&f.repo), json!(second));
    assert_eq!(f.symbol("a.py", "alpha"), alpha_before);
    f.symbol("b.py", "run");
    // Back to the first head set: no projection runs, the table follows.
    f.sync(&[("a.py", A)]);
    assert_eq!(f.current_episode(&f.repo), first);
    assert_eq!(f.table_episode(&f.repo), json!(first));
    assert!(
        f.sql("SELECT * FROM gf_symbol_heads WHERE relpath='b.py'")
            .is_empty()
    );
}

/// Under the PR scratch project: `graphicle pr` records commit
/// trees in the folder project's PR scratch project, so a commit tree that
/// holds the old bytes of a changed function never makes the folder
/// project's memory current again.
#[test]
fn a_pr_scratch_projection_never_moves_the_folder_projects_symbols() {
    let f = Fixture::new(&[("a.py", A)]);
    let memory = f.note(&[("file:a.py", "    return 1")]);
    let (alpha, old_revision) = f.symbol("a.py", "alpha");
    f.sync(&[("a.py", &A.replace("return 1", "return 10"))]);
    f.drain();
    let episode = f.table_episode(&f.repo);
    assert_eq!(
        reasons(&f.memory(&memory["id"])),
        [format!("symbol_changed:{alpha}")]
    );
    let scratch = f
        .store
        .pr_scratch_for(&f.workspace, &f.key, &f.repo)
        .unwrap();
    let scratch = s(&scratch, "id").to_string();
    f.sync_in(&scratch, &[("a.py", A)]);
    f.drain_in(&scratch);
    // The scratch project holds the old bytes under the same stable id.
    assert_eq!(
        f.symbol_in(&scratch, "a.py", "alpha"),
        (alpha.clone(), old_revision)
    );
    assert_eq!(f.table_episode(&f.repo), episode);
    assert_eq!(
        reasons(&f.memory(&memory["id"])),
        [format!("symbol_changed:{alpha}")]
    );
}

/// A check on a scripted history: edits outside the cited
/// symbols withhold fewer memories than the file rule, and no served memory
/// cites bytes that changed.
#[test]
fn scripted_edits_withhold_less_than_the_file_rule_and_never_serve_changed_bytes() {
    let functions = |values: [u32; 4]| -> String {
        values
            .iter()
            .enumerate()
            .map(|(i, v)| format!("def f{i}():\n    return {v}\n\n\n"))
            .collect()
    };
    let f = Fixture::new(&[("m.py", &functions([0, 1, 2, 3]))]);
    let quotes: Vec<(String, Value)> = (0..4)
        .map(|i| {
            let quote = format!("    return {i}");
            let memory = f.note(&[("file:m.py", &quote)]);
            (quote, memory["id"].clone())
        })
        .collect();
    let steps: Vec<(&str, Vec<(String, String)>)> = vec![
        ("edit f1", vec![("m.py".into(), functions([0, 10, 2, 3]))]),
        (
            "append f4",
            vec![(
                "m.py".into(),
                functions([0, 10, 2, 3]) + "def f4():\n    return 4\n",
            )],
        ),
        (
            "move the file",
            vec![(
                "pkg/m.py".into(),
                functions([0, 10, 2, 3]) + "def f4():\n    return 4\n",
            )],
        ),
        (
            "edit f3 and insert above",
            vec![(
                "pkg/m.py".into(),
                format!(
                    "import os\n\n\n{}def f4():\n    return 4\n",
                    functions([0, 10, 2, 30])
                ),
            )],
        ),
    ];
    let mut fewer = 0;
    for (name, files) in steps {
        let files: Vec<(&str, &str)> = files
            .iter()
            .map(|(p, t)| (p.as_str(), t.as_str()))
            .collect();
        f.sync(&files);
        f.drain();
        let (path, content) = files[0];
        let mut by_symbol = 0;
        for (quote, id) in &quotes {
            let memory = f.memory(id);
            if memory["stale"] == true {
                by_symbol += 1;
                continue;
            }
            // Served: the quoted line is unchanged in the file where the
            // function is now.
            assert!(
                content.contains(&format!("{quote}\n")) && path.ends_with("m.py"),
                "{name}: served {quote:?} whose bytes changed: {memory:#}"
            );
        }
        // The file rule withholds every memory of the changed file.
        let by_file = quotes.len();
        assert!(by_symbol <= by_file, "{name}");
        if by_symbol < by_file {
            fewer += 1;
        }
    }
    assert_eq!(fewer, 4, "every step left some memories current");
    // f1 and f3 changed: exactly their memories are withheld.
    let withheld: Vec<bool> = quotes
        .iter()
        .map(|(_, id)| f.memory(id)["stale"] == true)
        .collect();
    assert_eq!(withheld, [false, true, false, true]);
}

/// A refresh of a memory judged by symbols (`prepare_refresh`,
/// `propose_refresh`, approval): the refresh context holds the file a linked
/// symbol moved to; the files the draft does not cite keep their links and
/// rules (`symbols::carry`), their pending quotes take the memory's pins,
/// and an approval passes the draft's own pins on to the memory.
#[test]
fn a_refresh_keeps_the_symbol_links_of_the_files_it_does_not_cite() {
    const C: &str = "def gamma():\n    return 3\n";
    let f = Fixture::new(&[("a.py", A), ("b.py", B)]);
    // c.py arrives while its projection waits: its quote is pending.
    f.sync(&[("a.py", A), ("b.py", B), ("c.py", C)]);
    let memory = f.note_with(
        &[
            ("file:a.py", "    return 1"),
            ("file:b.py", "    return alpha() + beta()"),
            ("file:c.py", "    return 3"),
        ],
        json!({"body":"## Details\n\nRun adds alpha and beta.\n\nGamma stays three."}),
    );
    assert_eq!(
        memory["data"]["symbol_files"],
        json!({"a.py":"symbols","b.py":"symbols","c.py":"pending"})
    );
    f.drain();
    let (alpha, _) = f.symbol("a.py", "alpha");
    let (run, _) = f.symbol("b.py", "run");
    // a.py moves unchanged, run changes, a line goes in above gamma.
    let run_one = B.replace("beta()\n", "beta() + 1\n");
    let c_above = format!("import os\n\n\n{C}");
    f.sync(&[("lib/a.py", A), ("b.py", &run_one), ("c.py", &c_above)]);
    f.drain();
    let stale = f.memory(&memory["id"]);
    assert_eq!(reasons(&stale), [format!("symbol_changed:{run}")]);
    assert_eq!(stale["symbol_links"]["followed_move"], 1);

    let prepare = |id: &Value| {
        f.store
            .refresh_context(&f.workspace, &f.key, &f.args(json!({"memory_id":id})))
            .unwrap()
    };
    let propose = |prepared: &Value, text: &str, quote: &str| {
        let context = &prepared["context"];
        let section = &context["document"]["sections"][0];
        f.store
            .project_mutate(
                &f.workspace,
                &f.key,
                "learning.refresh.propose",
                &f.args(
                    json!({"memory_id":context["memory_id"],"context_sha256":context["sha256"],
                    "operations":[{"op":"replace_block","section_id":section["id"],
                        "block_id":section["blocks"][0]["id"],"text":text}],
                    "evidence":[{"id":"file:b.py","quote":quote}]}),
                ),
            )
            .unwrap()["drafts"][0]
            .clone()
    };
    let approve = |draft: &Value| {
        f.store
            .review_memory(
                &f.workspace,
                &f.key,
                &f.args(
                    json!({"memory_id":draft["id"],"expected_revision":draft["revision"],
                    "decision":"approve","maintainer":"Reviewer","reason":"Checked"}),
                ),
            )
            .unwrap()
    };
    let pins = |id: &Value| {
        f.sql(&format!(
            "SELECT relpath,sha256,byte_start,byte_end,state,covered,symbols FROM gf_symbol_pins WHERE memory_id='{}' ORDER BY relpath",
            id.as_str().unwrap()
        ))
    };

    // The context holds the file alpha moved to.
    let prepared = prepare(&memory["id"]);
    let sources: Vec<&str> = prepared["context"]["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|source| source["id"].as_str().unwrap())
        .collect();
    assert!(sources.contains(&"file:lib/a.py"), "{sources:?}");
    let draft = propose(
        &prepared,
        "Run adds alpha, beta and one.",
        "    return alpha() + beta() + 1",
    );
    // a.py and c.py are kept with their links and rules; c.py's pending
    // quote took the memory's pin, though the table no longer describes the
    // content it quotes.
    let data = &draft["data"];
    assert_eq!(
        data["symbol_files"],
        json!({"a.py":"symbols","b.py":"symbols","c.py":"pending"})
    );
    assert!(
        data["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["stable_id"] == json!(alpha) && l["rung"] == "quote"),
        "{data:#}"
    );
    let memory_pins = pins(&memory["id"]);
    assert_eq!(memory_pins.len(), 1);
    assert_eq!(memory_pins[0][4], "pinned");
    assert_eq!(pins(&draft["id"]), memory_pins);
    let listed = f.memory(&draft["id"]);
    assert_eq!(listed["stale"], false, "{listed:#}");
    assert_eq!(listed["symbol_links"]["followed_move"], 1);
    assert_eq!(
        listed["symbol_links"]["locations"]["file:a.py"][0]["id"],
        "file:lib/a.py"
    );
    approve(&listed);
    let refreshed = f.memory(&memory["id"]);
    assert_eq!(
        (&refreshed["revision"], &refreshed["stale"]),
        (&json!(2), &json!(false)),
        "{refreshed:#}"
    );

    // run changes again while its projection waits: the second refresh's
    // quote is pending, pinned by the projection.
    let run_two = B.replace("beta()\n", "beta() + 2\n");
    f.sync(&[("lib/a.py", A), ("b.py", &run_two), ("c.py", &c_above)]);
    let draft = propose(
        &prepare(&memory["id"]),
        "Run adds alpha, beta and two.",
        "    return alpha() + beta() + 2",
    );
    assert_eq!(draft["data"]["symbol_files"]["b.py"], "pending");
    f.drain();
    // A line above run: the table moves past the quoted content, so only
    // the draft's pin can pin the memory's copy of that quote.
    let run_two_above = run_two.replace("beta\n", "beta\nimport os\n");
    f.sync(&[
        ("lib/a.py", A),
        ("b.py", &run_two_above),
        ("c.py", &c_above),
    ]);
    f.drain();
    let listed = f.memory(&draft["id"]);
    assert_eq!(listed["stale"], false, "{listed:#}");
    approve(&listed);
    let pinned: Vec<Vec<Value>> = pins(&memory["id"])
        .into_iter()
        .filter(|row| row[0] == "b.py")
        .collect();
    assert_eq!(pinned.len(), 1, "{pinned:?}");
    assert_eq!(
        (&pinned[0][4], &pinned[0][5]),
        (&json!("pinned"), &json!(1))
    );
    assert!(pinned[0][6].as_str().unwrap().contains(&run));
    let refreshed = f.memory(&memory["id"]);
    assert_eq!(
        (&refreshed["revision"], &refreshed["stale"]),
        (&json!(3), &json!(false)),
        "{refreshed:#}"
    );

    // Edits inside alpha, where it moved, and inside run withhold it.
    f.sync(&[
        ("lib/a.py", &A.replace("return 1", "return 10")),
        ("b.py", &run_two_above.replace("+ 2", "+ 20")),
        ("c.py", &c_above),
    ]);
    f.drain();
    let mut withheld = reasons(&f.memory(&memory["id"]));
    withheld.sort();
    let mut expected = vec![
        format!("symbol_changed:{alpha}"),
        format!("symbol_changed:{run}"),
    ];
    expected.sort();
    assert_eq!(withheld, expected);
}

/// A quote found more than four times in a file keeps the file rule (the
/// other quotes' links are recorded); up to four occurrences link each
/// symbol. A memory with more than 200 links records none, so every file
/// keeps the file rule.
#[test]
fn repeated_quotes_and_too_many_links_keep_the_file_rule() {
    let functions = |values: &[u32]| -> String {
        values
            .iter()
            .enumerate()
            .map(|(i, v)| format!("def f{i}():\n    return {v}\n\n\n"))
            .collect()
    };
    let five = functions(&[7, 7, 7, 7, 7, 8]);
    let four = functions(&[7, 7, 7, 7, 8]);
    let f = Fixture::new(&[("five.py", &five), ("four.py", &four)]);
    // Alone, a quote found five times stores what a memory stored before
    // symbol links.
    let only = f.note(&[("file:five.py", "    return 7")]);
    assert!(only["data"].get("symbols").is_none(), "{only:#}");
    assert!(only["data"].get("symbol_files").is_none());
    let mixed = f.note(&[
        ("file:five.py", "    return 7"),
        ("file:five.py", "    return 8"),
    ]);
    assert_eq!(
        mixed["data"]["symbol_files"],
        json!({"five.py":"repeated_quote"})
    );
    let (f5, _) = f.symbol("five.py", "f5");
    let linked: Vec<&Value> = mixed["data"]["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| &l["stable_id"])
        .collect();
    assert_eq!(linked, [&json!(f5)]);
    let bound = f.note(&[("file:four.py", "    return 7")]);
    assert_eq!(bound["data"]["symbol_files"], json!({"four.py":"symbols"}));
    assert_eq!(bound["data"]["symbols"].as_array().unwrap().len(), 4);
    // Lines inserted above every function: only the symbol rule serves.
    f.sync(&[
        ("five.py", &format!("import os\n{five}")),
        ("four.py", &format!("import os\n{four}")),
    ]);
    f.drain();
    for withheld in [&only, &mixed] {
        assert_eq!(
            reasons(&f.memory(&withheld["id"])),
            ["source_changed_or_unavailable:file:five.py"]
        );
    }
    assert_eq!(f.memory(&bound["id"])["stale"], false);

    // One quote in each of 201 functions: 201 links, so none is recorded.
    let values: Vec<u32> = (1000..1201).collect();
    let many = functions(&values);
    let g = Fixture::new(&[("many.py", &many)]);
    let quotes: Vec<String> = values.iter().map(|v| format!("    return {v}")).collect();
    let cite = |n: usize| -> Vec<(&str, &str)> {
        quotes[..n]
            .iter()
            .map(|quote| ("file:many.py", quote.as_str()))
            .collect()
    };
    let over = g.note(&cite(201));
    assert!(over["data"].get("symbols").is_none());
    assert!(over["data"].get("symbol_files").is_none());
    let under = g.note(&cite(200));
    assert_eq!(under["data"]["symbols"].as_array().unwrap().len(), 200);
    assert_eq!(under["data"]["symbol_files"], json!({"many.py":"symbols"}));
    g.sync(&[("many.py", &format!("import os\n{many}"))]);
    g.drain();
    assert_eq!(
        reasons(&g.memory(&over["id"])),
        ["source_changed_or_unavailable:file:many.py"]
    );
    assert_eq!(g.memory(&under["id"])["stale"], false);
}

/// `memory.search` at a knowledge time reads a historical revision and
/// judges it by its own links and its memory's pins (`memory::reasons` with
/// the memory's id).
#[test]
fn a_historical_revision_is_judged_by_its_own_symbols() {
    let f = Fixture::queued(&[("a.py", A), ("b.py", B)]);
    let recall = json!({"name":"rotation","aliases":["duty owner"],"scopes":["project:larch"]});
    // Revision 1 quotes alpha before the projection: pinned by it.
    let first = f.note_with(&[("file:a.py", "    return 1")], json!({"recall":recall}));
    assert_eq!(first["data"]["symbol_files"], json!({"a.py":"pending"}));
    f.drain();
    // Revision 2 quotes run instead.
    f.note_with(
        &[("file:b.py", "    return alpha() + beta()")],
        json!({"memory_id":first["id"],"expected_revision":1,"recall":recall}),
    );
    let db = f.store.connect().unwrap();
    db.execute(
        "UPDATE repo_memory_revisions SET created_at=CASE revision WHEN 1 THEN '2026-01-01T00:00:00Z' ELSE '2026-03-01T00:00:00Z' END WHERE memory_id=?",
        [s(&first, "id")],
    )
    .unwrap();
    let search = |known: Option<&str>| {
        let mut args = json!({"question":"duty owner","scopes":["project:larch"]});
        if let Some(known) = known {
            args["known_at"] = json!(known);
        }
        f.store
            .memory_search(&f.workspace, &f.key, &f.args(args))
            .unwrap()
    };
    let historical = || search(Some("2026-02-01T00:00:00Z"));
    assert_eq!(historical()["memories"][0]["revision"], 1);
    // An edit of beta: revision 1 is still served.
    f.sync(&[("a.py", &A.replace("return 2", "return 20")), ("b.py", B)]);
    f.drain();
    let found = historical();
    assert_eq!(found["memories"][0]["revision"], 1, "{found:#}");
    // An edit inside alpha withholds revision 1; revision 2 is current.
    f.sync(&[("a.py", &A.replace("return 1", "return 10")), ("b.py", B)]);
    f.drain();
    let found = historical();
    assert!(
        found["memories"].as_array().unwrap().is_empty(),
        "{found:#}"
    );
    assert_eq!(found["withheld_unreviewed_or_stale"], 1);
    assert_eq!(search(None)["memories"][0]["revision"], 2);
}

/// The web app's memory form sends named files as `dependencies` and
/// the symbols picked from the code graph (`graph.as_of`) as `symbols`.
#[test]
fn the_web_memory_form_names_files_and_picked_symbols() {
    let script = include_str!("../../web/app.js");
    let form = script
        .split("function newNote() {")
        .nth(1)
        .and_then(|rest| rest.split("\nfunction symbolPicker(").next())
        .unwrap();
    assert!(
        form.contains("dependencies, ...(symbols.length ? {symbols} : {})"),
        "{form}"
    );
    let picker = script
        .split("function symbolPicker(form) {")
        .nth(1)
        .unwrap();
    assert!(picker.contains("command(\"graph.as_of\""), "{picker}");
}
