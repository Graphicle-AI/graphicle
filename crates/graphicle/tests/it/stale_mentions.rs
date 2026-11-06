//! Stale doc mentions: a document passage that names a code definition is
//! flagged once that definition changes after the passage last did, and the
//! flag clears when the passage changes. A renamed or deleted definition is
//! `missing`; a mention that resolves to several definitions is never
//! flagged. `graph.doc_links` shows the flags (on each mention, in the
//! coverage and with `stale: true`), and a recall excerpt of a flagged
//! passage carries one note line.
use graphicle_runtime::{
    doc_links, jobs,
    store::{Store, s},
};
use serde_json::{Value, json};
use tempfile::TempDir;

const WIRE: &str = "class Parser:\n    def read(self, line):\n        return parse_header(line)\n\n\ndef parse_header(line):\n    name, _, value = line.partition(\":\")\n    return name.strip().lower(), value.strip()\n";
const GUIDE: &str = "# Guide\n\nHeaders go through `parse_header` before anything else.\n\nThe reader is `Parser.read`, which calls it.\n";

struct Fixture {
    _dir: TempDir,
    store: Store,
    workspace: String,
    key: String,
    repo: String,
}

impl Fixture {
    fn new() -> Self {
        crate::edition_setup();
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let ws = store.create_workspace("Stale mentions").unwrap();
        let workspace = s(&ws, "id").to_string();
        let key = s(&ws, "capability").to_string();
        let repo = s(
            &store
                .register(
                    &workspace,
                    &key,
                    &json!({"project_key":"wire","name":"wire"}),
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
        }
    }
    /// Sync these files as the project's whole tree, then project it and
    /// run the `link` job.
    fn sync(&self, files: &[(&str, &str)]) {
        let state = self
            .store
            .project_status(
                &self.workspace,
                &self.key,
                &json!({"repository_id":self.repo}),
            )
            .unwrap();
        let manifest: Vec<Value> = files
            .iter()
            .map(|(path, text)| json!({"path":path,"sha256":graphicle_memory::text::sha256(text.as_bytes()),"bytes":text.len()}))
            .collect();
        let begin = self
            .store
            .sync_prepare(
                &self.workspace,
                &self.key,
                &json!({"repository_id":self.repo,"expected_snapshot":state["repository"]["snapshot"],"manifest":manifest}),
            )
            .unwrap();
        let upload: Vec<Value> = files
            .iter()
            .map(|(path, text)| json!({"path":path,"content":text}))
            .collect();
        self.store
            .sync_upload(
                &self.workspace,
                &self.key,
                &json!({"repository_id":self.repo,"sync_id":begin["sync_id"],"files":upload}),
            )
            .unwrap();
        self.store
            .sync_commit(
                &self.workspace,
                &self.key,
                &json!({"repository_id":self.repo,"sync_id":begin["sync_id"]}),
            )
            .unwrap();
        jobs::drain_projection(&self.store, &self.repo).unwrap();
    }
    fn read(&self, args: Value) -> Value {
        let mut args = args;
        args["repository_id"] = json!(self.repo);
        doc_links::read(&self.store, &self.workspace, &self.key, &args).unwrap()
    }
    /// Each mention of the guide: (text, stale state or null).
    fn flags(&self) -> Vec<(String, Value)> {
        self.read(json!({"document_id":"file:docs/guide.md"}))["mentions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| (s(m, "mention").to_string(), m["stale"]["state"].clone()))
            .collect()
    }
    fn flag(&self, mention: &str) -> Value {
        self.flags()
            .into_iter()
            .find(|(text, _)| text == mention)
            .map(|(_, flag)| flag)
            .unwrap_or_else(|| panic!("no mention {mention}: {:?}", self.flags()))
    }
    fn context(&self, question: &str) -> Value {
        self.store
            .project_context(
                &self.workspace,
                &self.key,
                &json!({"repository_id":self.repo,"question":question}),
            )
            .unwrap()
    }
}

#[test]
fn a_mention_is_flagged_when_its_definition_changes_and_cleared_when_its_passage_does() {
    let f = Fixture::new();
    f.sync(&[("src/wire.py", WIRE), ("docs/guide.md", GUIDE)]);
    // First read: the baselines are the definitions as they are now.
    assert_eq!(
        f.flags(),
        [
            ("parse_header".to_string(), Value::Null),
            ("Parser.read".to_string(), Value::Null)
        ]
    );
    assert_eq!(f.read(json!({"stale":true}))["count"], 0);

    // The body of parse_header changes: its mention is flagged, the other is not.
    let changed = WIRE.replace("value.strip()", "value.strip(\" \")");
    f.sync(&[("src/wire.py", &changed), ("docs/guide.md", GUIDE)]);
    assert_eq!(f.flag("parse_header"), "changed");
    assert_eq!(f.flag("Parser.read"), Value::Null);
    let listed = f.read(json!({"stale":true}));
    assert_eq!(listed["count"], 1, "{listed}");
    let item = &listed["stale"][0];
    assert_eq!(item["document_id"], "file:docs/guide.md");
    assert_eq!(item["mention"], "parse_header");
    assert_eq!(item["line"], 3);
    assert_eq!(item["state"], "changed");
    assert_eq!(item["was"]["relpath"], "src/wire.py");
    assert_eq!(item["now"]["relpath"], "src/wire.py");
    assert_ne!(item["was"]["revision"], item["now"]["revision"]);
    assert!(item["since"].is_string());
    let coverage = f.read(json!({}));
    assert_eq!(
        coverage["coverage"]["total"]["stale_changed"], 1,
        "{coverage}"
    );

    // Recall: the excerpt of the flagged passage says so in one line.
    let packet = f.context("where do headers go through parse_header before anything else");
    assert_eq!(packet["stale_mentions"]["status"], "checked", "{packet}");
    assert!(
        packet["stale_mentions"]["excerpts_marked"]
            .as_u64()
            .unwrap()
            >= 1,
        "{}",
        packet["stale_mentions"]
    );
    let markdown = s(&packet, "markdown");
    assert!(
        markdown.contains(
            "Note: may be stale: this passage names `parse_header` (src/wire.py), which changed after the passage was written"
        ),
        "{markdown}"
    );

    // An edit to another passage keeps the flag.
    let other = GUIDE.replace("which calls it", "which calls the parser");
    f.sync(&[("src/wire.py", &changed), ("docs/guide.md", &other)]);
    assert_eq!(f.flag("parse_header"), "changed");

    // An edit to the passage itself clears it.
    let fixed = other.replace("before anything else", "before any other step");
    f.sync(&[("src/wire.py", &changed), ("docs/guide.md", &fixed)]);
    assert_eq!(f.flag("parse_header"), Value::Null);
    assert_eq!(f.read(json!({"stale":true}))["count"], 0);

    // A renamed definition: the mention no longer resolves and is missing.
    let renamed = changed.replace("parse_header", "parse_head");
    f.sync(&[("src/wire.py", &renamed), ("docs/guide.md", &fixed)]);
    assert_eq!(f.flag("parse_header"), "missing");
    let listed = f.read(json!({"stale":true}));
    let missing = listed["stale"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["mention"] == "parse_header")
        .unwrap_or_else(|| panic!("{listed}"));
    assert_eq!(missing["state"], "missing");
    assert!(missing.get("now").is_none());
    // The rename changed the body of Parser.read too, so the excerpt's one
    // note names both.
    assert_eq!(f.flag("Parser.read"), "changed");
    let packet = f.context("where do headers go through parse_header before any other step");
    assert!(
        s(&packet, "markdown").contains(
            "this passage names code that changed after it was written: `parse_header` (no longer defined), `Parser.read` (src/wire.py)"
        ),
        "{}",
        s(&packet, "markdown")
    );
}

#[test]
fn a_mention_of_several_definitions_is_never_flagged() {
    let f = Fixture::new();
    f.sync(&[("src/wire.py", WIRE), ("docs/guide.md", GUIDE)]);
    assert_eq!(f.flag("Parser.read"), Value::Null);
    // Parser.read changes and a second Parser.read appears: the mention is
    // ambiguous now, and ambiguous mentions are not flagged.
    let changed = WIRE.replace(
        "return parse_header(line)",
        "return parse_header(line.strip())",
    );
    let other = "class Parser:\n    def read(self, line):\n        return line\n";
    f.sync(&[
        ("src/wire.py", &changed),
        ("src/other.py", other),
        ("docs/guide.md", GUIDE),
    ]);
    let mentions = f.read(json!({"document_id":"file:docs/guide.md"}))["mentions"].clone();
    let read = mentions
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["mention"] == "Parser.read")
        .unwrap()
        .clone();
    assert_eq!(read["state"], "ambiguous", "{mentions}");
    assert!(read.get("stale").is_none(), "{read}");
    // The unrelated mention keeps its own judgment: parse_header did not change.
    assert_eq!(f.flag("parse_header"), Value::Null);
    // Once the second definition is gone, the change shows.
    f.sync(&[("src/wire.py", &changed), ("docs/guide.md", GUIDE)]);
    assert_eq!(f.flag("Parser.read"), "changed");
}

#[test]
fn a_document_that_is_removed_takes_its_baselines() {
    let f = Fixture::new();
    f.sync(&[("src/wire.py", WIRE), ("docs/guide.md", GUIDE)]);
    let count = || {
        f.store
            .read(|db| {
                Ok(graphicle_runtime::store::one(
                    db,
                    "SELECT count(*) AS n FROM gf_doc_mention_baselines",
                    [],
                )?
                .unwrap())
            })
            .unwrap()["n"]
            .as_i64()
            .unwrap()
    };
    assert_eq!(count(), 2);
    f.sync(&[("src/wire.py", WIRE)]);
    assert_eq!(count(), 0);
}
