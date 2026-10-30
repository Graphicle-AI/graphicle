//! Graphicle Core limits: generous defaults that whoever runs Graphicle
//! sets (`limits.*` in the configuration file, `GRAPHICLE_MAX_*` in the
//! environment). No single file fails a sync: a file past a per-file limit
//! is listed with the reason and its text stays searchable. A project past a
//! project limit is refused with a message that names the count, the limit,
//! the setting that raises it and how to leave paths out. A local sync sends
//! its manifest and content in batches under the request caps.
use graphicle_runtime::limits::{Limits, grouped};
use graphicle_runtime::local;
use graphicle_runtime::store::{Store, rows, s};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Instant;
use tempfile::TempDir;

struct Fixture {
    _dir: TempDir,
    store: Store,
    session: local::Session,
    root: PathBuf,
}

impl Fixture {
    fn new(limits: Limits) -> Self {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path().join("store"))
            .unwrap()
            .with_limits(limits);
        let session = local::session(&store, true).unwrap();
        let root = dir.path().canonicalize().unwrap().join("project");
        std::fs::create_dir_all(&root).unwrap();
        Self {
            _dir: dir,
            store,
            session,
            root,
        }
    }
    fn write(&self, path: &str, content: impl AsRef<[u8]>) {
        let target = self.root.join(path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(target, content).unwrap();
    }
    fn sync(&mut self) -> graphicle_runtime::Result<Value> {
        local::sync(&self.store, &mut self.session, &self.root, None)
    }
    /// The revision and analysis stored for `path` at the project's heads.
    fn analysis(&self, repository: &str, path: &str) -> (String, Value) {
        let row = self
            .store
            .read(|db| {
                rows(
                    db,
                    "SELECT p.revision,p.data FROM repo_parses p JOIN repo_heads h ON h.repository_id=p.repository_id AND h.path=p.path AND h.sha256=p.sha256 WHERE h.repository_id=? AND h.path=?",
                    rusqlite::params![repository, path],
                )
            })
            .unwrap()
            .pop()
            .unwrap_or_else(|| panic!("no analysis of {path}"));
        (
            s(&row, "revision").to_string(),
            serde_json::from_str(s(&row, "data")).unwrap(),
        )
    }
    /// Whether a context packet for `question` cites `path`.
    fn cites(&self, repository: &str, question: &str, path: &str) -> bool {
        let packet = self
            .store
            .project_context(
                &self.session.workspace,
                &self.session.capability,
                &json!({"repository_id":repository,"question":question}),
            )
            .unwrap();
        packet["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|evidence| evidence["id"] == json!(format!("file:{path}")))
    }
}

/// Per-file limits small enough for small fixture files.
fn small() -> Limits {
    Limits {
        file_bytes: 64 * 1024,
        analysis_bytes: 16 * 1024,
        file_calls: 50,
        file_nodes: 2_000,
        ..Limits::default()
    }
}

fn paths(list: &Value) -> Vec<&str> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|item| s(item, "path"))
        .collect()
}

#[test]
fn an_oversized_file_of_each_kind_is_listed_and_searchable_and_the_sync_succeeds() {
    let mut f = Fixture::new(small());
    let normal = "import os\n\ndef helper(name):\n    return os.path.join(\"lighthouse\", name)\n\ndef main():\n    return helper(\"keeper\")\n";
    let big: String = std::iter::once("# The quasar marker lives here.\n".to_string())
        .chain((0..800).map(|i| format!("def quasar_{i}():\n    return {i}\n")))
        .collect();
    let calls = format!(
        "# The nebula marker lives here.\ndef main():\n{}",
        "    work()\n".repeat(60)
    );
    let nodes = format!(
        "# The pulsar marker lives here.\nvalues = [{}]\n",
        "1, ".repeat(3000)
    );
    let huge = "The comet marker lives here.\n".repeat(2600);
    assert!(big.len() > 16 * 1024 && big.len() < 64 * 1024);
    assert!(nodes.len() < 16 * 1024 && huge.len() > 64 * 1024);
    f.write("normal.py", normal);
    f.write("big.py", &big);
    f.write("calls.py", &calls);
    f.write("nodes.py", &nodes);
    f.write("huge.txt", &huge);
    f.write("broken.docx", "this is not an Office archive");
    // A binary over the per-file size would not have been indexed: counted
    // under `excluded`, not listed under `skipped`.
    f.write("weights.bin", vec![0u8; 70 * 1024]);

    let synced = f.sync().unwrap();
    let repository = s(&synced, "repository_id").to_string();
    assert_eq!(synced["files"], 4, "{synced}");
    assert_eq!(synced["excluded"], 3, "{synced}");
    assert_eq!(
        synced["limited"],
        json!([
            {"path":"big.py","limit":"size","reason":format!(
                "Too large to analyse ({} bytes; limit 16,384); searchable as text only",
                grouped(big.len())
            )},
            {"path":"calls.py","limit":"calls","reason":"Call list truncated at 50 calls"},
            {"path":"nodes.py","limit":"nodes","reason":"Analysis stopped at 2,000 syntax nodes"},
        ]),
        "{synced}"
    );
    assert_eq!(
        synced["skipped"],
        json!([
            {"path":"broken.docx","reason":"Document not converted: Invalid Office/document archive"},
            {"path":"huge.txt","reason":format!(
                "Too large to index ({} bytes; limit 65,536); left out",
                grouped(huge.len())
            )},
        ]),
        "{synced}"
    );
    assert_eq!(
        local::limits_note(&synced).unwrap(),
        "graphicle: 3 files analysed in part and 2 files skipped; \"limited\" and \"skipped\" in the result list each file with the reason"
    );

    // The normal file is analysed in full, as before.
    let (revision, analysis) = f.analysis(&repository, "normal.py");
    assert_eq!(revision, graphicle_memory::structure::REVISION);
    assert_eq!(analysis["complete"], true, "{analysis}");
    assert!(analysis.get("limited").is_none());
    let names: Vec<&str> = analysis["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .map(|symbol| s(symbol, "name"))
        .collect();
    assert_eq!(names, ["helper", "main"]);
    assert_eq!(analysis["calls"].as_array().unwrap().len(), 2);
    // The others are analysed as far as their limits allow.
    let limited_revision = graphicle_memory::structure::limited_revision(&small().analysis());
    let (revision, analysis) = f.analysis(&repository, "calls.py");
    assert_eq!(revision, limited_revision);
    assert_eq!(analysis["calls"].as_array().unwrap().len(), 50);
    assert_eq!(analysis["symbols"][0]["name"], "main");
    let (_, analysis) = f.analysis(&repository, "big.py");
    assert_eq!(analysis["symbols"], json!([]));
    assert_eq!(analysis["complete"], false);

    // Their text is indexed and searchable; the file left out is not.
    for (question, path) in [
        ("Where does the quasar marker live?", "big.py"),
        ("Where does the nebula marker live?", "calls.py"),
        ("Where does the pulsar marker live?", "nodes.py"),
        ("Where does the lighthouse keeper live?", "normal.py"),
    ] {
        assert!(f.cites(&repository, question, path), "{question}");
    }
    assert!(!f.cites(&repository, "Where does the comet marker live?", "huge.txt"));

    // An unchanged folder is replayed and still lists them.
    let again = f.sync().unwrap();
    assert_eq!(again["replayed"], true, "{again}");
    assert_eq!(again["limited"], synced["limited"]);
    assert_eq!(again["skipped"], synced["skipped"]);

    // Raised analysis limits redo the limited analyses of the unchanged
    // folder, and only those: the complete analysis of the normal file is
    // not redone (its marked row survives).
    f.store
        .transaction(|db| {
            db.execute(
                "UPDATE repo_parses SET data=json_set(data,'$.marked',1) WHERE path='normal.py'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    f.store = f.store.clone().with_limits(Limits {
        file_bytes: 64 * 1024,
        ..Limits::default()
    });
    let raised = f.sync().unwrap();
    assert_eq!(raised["replayed"], false, "{raised}");
    assert_eq!(raised["snapshot"], synced["snapshot"]);
    assert!(raised.get("limited").is_none(), "{raised}");
    assert_eq!(paths(&raised["skipped"]), ["broken.docx", "huge.txt"]);
    assert_eq!(raised["files"], 4);
    let (revision, analysis) = f.analysis(&repository, "calls.py");
    assert_eq!(revision, graphicle_memory::structure::REVISION);
    assert_eq!(analysis["calls"].as_array().unwrap().len(), 60);
    assert_eq!(analysis["complete"], true);
    assert_eq!(f.analysis(&repository, "normal.py").1["marked"], 1);
    assert_eq!(
        local::limits_note(&raised).unwrap(),
        "graphicle: 2 files skipped; \"skipped\" in the result lists each file with the reason"
    );
}

#[test]
fn a_configured_project_limit_refuses_with_the_count_the_limit_the_setting_and_the_way_out() {
    let mut f = Fixture::new(Limits {
        project_files: 3,
        ..Limits::default()
    });
    for name in ["a", "b", "c", "d"] {
        f.write(&format!("{name}.txt"), format!("Note {name}.\n"));
    }
    let refused = f.sync().unwrap_err();
    assert_eq!(
        refused.message,
        "This project is too large to sync: it has 4 text files (the limit is 3). Raise the limit with limits.project_files in the configuration file (models.json) or the GRAPHICLE_MAX_PROJECT_FILES environment variable, or leave untracked paths out with .gitignore or .git/info/exclude (they cover only files Git does not track; graphicle config set sync.tracked_ignored false, or GRAPHICLE_SYNC_TRACKED_IGNORED=0, leaves out the tracked files they match too), or set up a subfolder: graphicle setup path/to/subfolder"
    );
    // Past the limit the rest is counted without being read.
    f.write("e.txt", "Note e.\n");
    let refused = f.sync().unwrap_err();
    assert!(
        refused
            .message
            .contains("it has about 5 text files (the limit is 3)"),
        "{}",
        refused.message
    );
    // Leaving paths out brings the project within the limit (the
    // `.gitignore` is one of its three files).
    f.write(".gitignore", "c.txt\nd.txt\ne.txt\n");
    let synced = f.sync().unwrap();
    assert_eq!(synced["files"], 3, "{synced}");
    assert!(synced.get("limited").is_none() && synced.get("skipped").is_none());

    let mut f = Fixture::new(Limits {
        project_bytes: 10,
        ..Limits::default()
    });
    f.write("a.txt", "Twenty bytes of text");
    assert_eq!(
        f.sync().unwrap_err().message,
        "This project is too large to sync: it has 20 bytes of text (the limit is 10 bytes). Raise the limit with limits.project_bytes in the configuration file (models.json) or the GRAPHICLE_MAX_PROJECT_BYTES environment variable, or leave untracked paths out with .gitignore or .git/info/exclude (they cover only files Git does not track; graphicle config set sync.tracked_ignored false, or GRAPHICLE_SYNC_TRACKED_IGNORED=0, leaves out the tracked files they match too), or set up a subfolder: graphicle setup path/to/subfolder"
    );
}

/// The `sync.*` protocol takes a manifest of any size in parts under the
/// request caps; nothing a reader sees changes before the commit, a failed
/// step commits nothing, and the steps can be retried.
#[test]
fn a_manifest_in_parts_commits_nothing_until_the_commit_and_retries_are_idempotent() {
    let f = Fixture::new(Limits::default());
    let (workspace, key) = (f.session.workspace.as_str(), f.session.capability.as_str());
    let repository = s(
        &f.store
            .register(
                workspace,
                key,
                &json!({"project_key":"parts","name":"Parts"}),
            )
            .unwrap(),
        "id",
    )
    .to_string();
    let file = |i: usize| {
        (
            format!("src/m{i}.py"),
            format!("def m{i}():\n    return {i}\n"),
        )
    };
    let entry = |i: usize| {
        let (path, content) = file(i);
        json!({"path":path,"sha256":graphicle_memory::text::sha256(content.as_bytes()),"bytes":content.len()})
    };
    let heads = || {
        f.store
            .read(|db| {
                Ok(rows(
                    db,
                    "SELECT count(*) AS n FROM repo_heads WHERE repository_id=?",
                    [&repository],
                )?[0]["n"]
                    .as_i64()
                    .unwrap())
            })
            .unwrap()
    };
    let call = |operation: &str, mut args: Value| {
        args["repository_id"] = json!(repository);
        match operation {
            "prepare" => f.store.sync_prepare(workspace, key, &args),
            "upload" => f.store.sync_upload(workspace, key, &args),
            _ => f.store.sync_commit(workspace, key, &args),
        }
    };
    // One request carries at most 5,000 entries.
    let oversized: Vec<Value> = (0..5001).map(entry).collect();
    let refused = call(
        "prepare",
        json!({"manifest":oversized,"expected_snapshot":""}),
    )
    .unwrap_err();
    assert!(
        refused.message.contains("send a larger manifest in parts"),
        "{}",
        refused.message
    );
    // A sync whose manifest is still coming can be aborted.
    let first = call(
        "prepare",
        json!({"manifest":[entry(0)],"manifest_complete":false,"expected_snapshot":""}),
    )
    .unwrap();
    f.store
        .project_mutate(
            workspace,
            key,
            "sync.abort",
            &json!({"repository_id":repository,"sync_id":first["sync_id"]}),
        )
        .unwrap();
    assert!(
        call(
            "prepare",
            json!({"manifest":[entry(1)],"sync_id":first["sync_id"]})
        )
        .unwrap_err()
        .message
        .contains("not waiting for more of its manifest")
    );
    // Three parts of two entries.
    let mut prepared = Value::Null;
    for part in 0..3 {
        let mut args = json!({"manifest":[entry(2 * part), entry(2 * part + 1)]});
        if part < 2 {
            args["manifest_complete"] = json!(false);
        }
        if part == 0 {
            args["expected_snapshot"] = json!("");
        } else {
            args["sync_id"] = prepared["sync_id"].clone();
        }
        prepared = call("prepare", args).unwrap();
        if part < 2 {
            assert_eq!(prepared["manifest_complete"], false);
            assert!(
                call("commit", json!({"sync_id":prepared["sync_id"]}))
                    .unwrap_err()
                    .message
                    .contains("rest of the manifest")
            );
        }
    }
    assert_eq!(prepared["missing"].as_array().unwrap().len(), 6);
    let sync_id = prepared["sync_id"].clone();
    // Failed steps leave the heads as they were.
    assert!(
        call("commit", json!({"sync_id":sync_id}))
            .unwrap_err()
            .message
            .contains("Upload missing sources")
    );
    let wrong = json!([{"path":"src/m0.py","content":"def other():\n    pass\n"}]);
    assert!(call("upload", json!({"sync_id":sync_id,"files":wrong})).is_err());
    let upload = |range: std::ops::Range<usize>| {
        let files: Vec<Value> = range
            .map(|i| {
                let (path, content) = file(i);
                json!({"path":path,"content":content})
            })
            .collect();
        call("upload", json!({"sync_id":sync_id,"files":files})).unwrap()
    };
    upload(0..3);
    assert!(call("commit", json!({"sync_id":sync_id})).is_err());
    assert_eq!(heads(), 0);
    // An upload repeated is accepted again; the commit moves every head.
    upload(0..6);
    upload(3..6);
    let committed = call("commit", json!({"sync_id":sync_id})).unwrap();
    assert_eq!(committed["files"], 6, "{committed}");
    assert_eq!(heads(), 6);
    let again = call("commit", json!({"sync_id":sync_id})).unwrap();
    assert_eq!(again["replayed"], true, "{again}");
    assert_eq!(heads(), 6);
}

/// 12,000 small files: past the 5,000-file project limit of earlier builds
/// and the request caps; the manifest goes in three parts and the content in
/// twelve uploads. The wall time is printed (`--nocapture`).
#[test]
fn twelve_thousand_small_files_sync() {
    let mut f = Fixture::new(Limits::default());
    for i in 0..12_000 {
        f.write(
            &format!("src/pkg{}/mod{i}.py", i / 500),
            format!("def f{i}(x):\n    return g{i}(x) + {i}\n"),
        );
    }
    let started = Instant::now();
    let synced = f.sync().unwrap();
    let elapsed = started.elapsed();
    eprintln!(
        "twelve_thousand_small_files_sync: synced {} files in {:.1} s",
        synced["files"],
        elapsed.as_secs_f64()
    );
    assert_eq!(synced["files"], 12_000, "{synced}");
    assert!(synced.get("limited").is_none() && synced.get("skipped").is_none());
    let repository = s(&synced, "repository_id").to_string();
    let (revision, analysis) = f.analysis(&repository, "src/pkg23/mod11999.py");
    assert_eq!(revision, graphicle_memory::structure::REVISION);
    assert_eq!(analysis["calls"][0]["callee"], "g11999");
    // Before its code graph is built, a project this large reads without
    // file relationships instead of failing.
    let status = f
        .store
        .project_status(
            &f.session.workspace,
            &f.session.capability,
            &json!({"repository_id":repository}),
        )
        .unwrap();
    assert_eq!(status["files"], 12_000);
    assert_eq!(status["graph"]["limited"], "files", "{}", status["graph"]);
    assert!(f.cites(
        &repository,
        "What does f11999 return?",
        "src/pkg23/mod11999.py"
    ));
    let again = f.sync().unwrap();
    assert_eq!(again["replayed"], true);
    assert_eq!(again["files"], 12_000);
}

/// The limits come from the configuration file's `limits` and from
/// `GRAPHICLE_MAX_*`, which wins; bad values are refused with the setting
/// named. `graphicle sync` prints one line when files were limited.
#[test]
fn the_command_line_takes_limits_from_the_configuration_file_and_the_environment() {
    let dir = TempDir::new().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let (home, data, folder) = (base.join("home"), base.join("data"), base.join("project"));
    for path in [&home, &data, &folder] {
        std::fs::create_dir_all(path).unwrap();
    }
    for name in ["a", "b"] {
        std::fs::write(
            folder.join(format!("{name}.txt")),
            format!("Note {name}.\n"),
        )
        .unwrap();
    }
    std::fs::write(
        folder.join("main.py"),
        "def main():\n    one()\n    two()\n",
    )
    .unwrap();
    let sync = |environment: &[(&str, &str)]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_graphicle"));
        command
            .current_dir(&folder)
            .env("HOME", &home)
            .arg("--data-dir")
            .arg(&data)
            .arg("sync")
            .arg(&folder)
            .arg("--no-graph")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for name in ["GRAPHICLE_MAX_PROJECT_FILES", "GRAPHICLE_MAX_FILE_CALLS"] {
            command.env_remove(name);
        }
        for (name, value) in environment {
            command.env(name, value);
        }
        let output = crate::fork_guard::spawn(&mut command)
            .unwrap()
            .wait_with_output()
            .unwrap();
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };
    let refusal = "it has 3 text files (the limit is 2)";
    let (ok, _, stderr) = sync(&[("GRAPHICLE_MAX_PROJECT_FILES", "2")]);
    assert!(!ok && stderr.contains(refusal), "{stderr}");
    assert!(stderr.contains("GRAPHICLE_MAX_PROJECT_FILES environment variable"));
    std::fs::write(
        data.join("models.json"),
        r#"{"limits":{"project_files":2}}"#,
    )
    .unwrap();
    let (ok, _, stderr) = sync(&[]);
    assert!(!ok && stderr.contains(refusal), "{stderr}");
    // The environment wins over the file.
    let (ok, stdout, stderr) = sync(&[
        ("GRAPHICLE_MAX_PROJECT_FILES", "10"),
        ("GRAPHICLE_MAX_FILE_CALLS", "1"),
    ]);
    assert!(ok, "{stderr}");
    let synced: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(synced["files"], 3);
    assert_eq!(
        synced["limited"],
        json!([{"path":"main.py","limit":"calls","reason":"Call list truncated at 1 call"}])
    );
    let note: Vec<&str> = stderr
        .lines()
        .filter(|line| line.contains("analysed in part"))
        .collect();
    assert_eq!(
        note,
        [
            "graphicle: 1 file analysed in part; \"limited\" in the result lists each file with the reason"
        ],
        "{stderr}"
    );
    // Bad values name the setting and what it takes.
    std::fs::write(
        data.join("models.json"),
        r#"{"limits":{"project_files":"lots"}}"#,
    )
    .unwrap();
    let (ok, _, stderr) = sync(&[]);
    assert!(!ok, "{stderr}");
    assert!(
        stderr.contains("Invalid limits.project_files in the configuration file: \"lots\". Use a whole number from 1 to 10,000,000"),
        "{stderr}"
    );
    std::fs::remove_file(data.join("models.json")).unwrap();
    let (ok, _, stderr) = sync(&[("GRAPHICLE_MAX_FILE_CALLS", "0")]);
    assert!(!ok, "{stderr}");
    assert!(
        stderr.contains(
            "Invalid GRAPHICLE_MAX_FILE_CALLS: \"0\". Use a whole number from 1 to 10,000,000"
        ),
        "{stderr}"
    );
}

/// An entry that cannot be read is listed under `skipped` with the reason
/// and the sync goes on (only the project folder itself must be readable).
#[cfg(unix)]
#[test]
fn an_unreadable_file_is_listed_and_the_sync_goes_on() {
    use std::os::unix::fs::PermissionsExt;
    let mut f = Fixture::new(Limits::default());
    f.write("main.py", "print('hi')\n");
    f.write("locked.py", "secret = 1\n");
    let locked = f.root.join("locked.py");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&locked).is_ok() {
        // Running as a user who reads every file (root): nothing to test.
        return;
    }
    let synced = f.sync().unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(synced["files"], 1, "{synced}");
    assert_eq!(
        synced["skipped"],
        json!([{"path":"locked.py","reason":"Could not read: Permission denied (os error 13)"}]),
        "{synced}"
    );
}

/// Connector and uploaded documents take as much text as a converted
/// document (`limits.document_text_bytes`, 8 MiB; earlier builds 512 KiB).
/// Past it the refusal names the document, the limit and the setting.
#[test]
fn connector_and_uploaded_documents_take_the_document_text_limit() {
    use base64::Engine;
    let mut f = Fixture::new(Limits::default());
    f.write("main.py", "print('hi')\n");
    let synced = f.sync().unwrap();
    let project = s(&synced, "repository_id").to_string();
    let (ws, key) = (f.session.workspace.clone(), f.session.capability.clone());
    let connector = f
        .store
        .connector_add(
            &ws,
            &key,
            &json!({"repository_id":project,"kind":"jsonl","name":"Documents",
                "config":{"acl":{"workspace_readers":false,"credentials":[]}}}),
        )
        .unwrap();
    // connector.import: 600 KiB of text.
    let long = "ledger ".repeat(600 * 1024 / 7);
    assert!(long.len() > 512 * 1024);
    let imported = f
        .store
        .import_documents(
            &ws,
            &key,
            &json!({"repository_id":project,"connector_id":connector["id"],"mode":"delta",
                "documents":[{"id":"long","title":"Long","content":long}]}),
        )
        .unwrap();
    assert_eq!(imported["imported"], 1, "{imported}");
    let over = "x".repeat(graphicle_runtime::documents::max_text() + 1);
    let refused = f
        .store
        .import_documents(
            &ws,
            &key,
            &json!({"repository_id":project,"connector_id":connector["id"],"mode":"delta",
                "documents":[{"id":"huge","title":"Huge","content":over}]}),
        )
        .unwrap_err()
        .message;
    assert!(
        refused.starts_with("Source document too large: source document huge has 8.0 MiB of text (the limit is 8 MiB); raise the limit with limits.document_text_bytes in the configuration file or GRAPHICLE_MAX_DOCUMENT_TEXT_BYTES"),
        "{refused}"
    );
    // documents.import: a 1 MiB HTML page.
    let html = format!(
        "<html><body><p>{}</p></body></html>",
        "ledger ".repeat((1 << 20) / 7)
    );
    let uploaded = f
        .store
        .import_binary(
            &ws,
            &key,
            &json!({"repository_id":project,"connector_id":connector["id"],"name":"page.html",
                "base64":base64::engine::general_purpose::STANDARD.encode(html)}),
        )
        .unwrap();
    assert!(uploaded["source_id"].is_string(), "{uploaded}");
}

/// A folder's HTML is read as it is when it is UTF-8 text within
/// `file_bytes`; one past `file_bytes` (up to `document_bytes`) or not UTF-8
/// (a Latin-1 or Shift-JIS page, which earlier versions indexed) is
/// converted to text, as every HTML file was before 0.1.10, and nothing is
/// dropped.
#[test]
fn a_folders_html_past_the_file_limit_or_not_utf8_is_converted() {
    let mut f = Fixture::new(small());
    let page = "<html><body><p>The aurora marker lives here.</p></body></html>\n";
    let big = format!(
        "<html><body>\n{}</body></html>\n",
        "<p>The meteor marker lives here.</p>\n".repeat(1_800)
    );
    assert!(big.len() > 64 * 1024);
    f.write("page.html", page);
    f.write("big.html", &big);
    f.write(
        "latin.htm",
        b"<html><body><p>Caf\xe9 au lait: the zenith marker lives here.</p></body></html>\n",
    );
    f.write(
        "sjis.html",
        b"<html><body><p>\x82\xa0\x82\xa2: the nadir marker lives here.</p></body></html>\n",
    );
    let synced = f.sync().unwrap();
    assert_eq!(synced["binary_documents"], 3, "{synced}");
    assert_eq!(synced["excluded"], 0, "{synced}");
    let repository = s(&synced, "repository_id").to_string();
    let stored = |path: &str| -> (String, i64) {
        f.store
            .read(|db| {
                Ok(db.query_row(
                    "SELECT o.content,(SELECT count(*) FROM am_binary_heads b WHERE b.repository_id=h.repository_id AND b.path=h.path) FROM repo_heads h JOIN repo_objects o ON o.repository_id=h.repository_id AND o.sha256=h.sha256 WHERE h.repository_id=? AND h.path=?",
                    rusqlite::params![repository, path],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                )?)
            })
            .unwrap()
    };
    assert_eq!(stored("page.html"), (page.to_string(), 0));
    for (path, marker) in [
        ("big.html", "meteor marker"),
        ("latin.htm", "zenith marker"),
        ("sjis.html", "nadir marker"),
    ] {
        let (text, converted) = stored(path);
        assert!(
            text.contains(marker) && !text.contains("<p>"),
            "{path}: {text}"
        );
        assert_eq!(converted, 1, "{path}");
    }
    // Past document_bytes, an HTML file is left out and listed.
    f.store = f.store.clone().with_limits(Limits {
        document_bytes: 32 * 1024,
        ..small()
    });
    let synced = f.sync().unwrap();
    let skipped = paths(&synced["skipped"]);
    assert!(skipped.contains(&"big.html"), "{synced}");
}

/// On a file system that ignores case (macOS, Windows), Git matches ignore
/// patterns without case (`core.ignorecase`) while the sync's walk matches
/// them with case: a tracked `src/Foo.js` that `.gitignore`'s `foo.js`
/// matches only that way is listed by both. The sync reads it once: its bytes
/// and the tracked-ignored count are not doubled. Setting
/// `core.ignorecase` simulates such a file system anywhere.
#[test]
fn a_tracked_file_an_ignore_pattern_matches_only_without_case_is_read_once() {
    let mut f = Fixture::new(Limits::default());
    let git = |args: &[&str]| {
        let status = Command::new("git")
            .arg("-C")
            .arg(&f.root)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    };
    git(&["init", "-q"]);
    git(&["config", "core.ignorecase", "true"]);
    let ignore = "foo.js\n";
    let source = "export const foo = 1;\n";
    f.write(".gitignore", ignore);
    f.write("src/Foo.js", source);
    git(&["add", "-f", ".gitignore", "src/Foo.js"]);
    let synced = f.sync().unwrap();
    assert_eq!(synced["files"], 2, "{synced}");
    assert_eq!(synced["bytes"], ignore.len() + source.len(), "{synced}");
    assert_eq!(synced["tracked_ignored"], 1, "{synced}");
}

/// Two hard links to one file are two paths of the project: the sync reads
/// both, as it reads two copies.
#[cfg(unix)]
#[test]
fn two_hard_links_to_one_file_are_both_read() {
    let mut f = Fixture::new(Limits::default());
    let source = "export const shared = 1;\n";
    f.write("src/a.js", source);
    std::fs::hard_link(f.root.join("src/a.js"), f.root.join("src/b.js")).unwrap();
    let synced = f.sync().unwrap();
    assert_eq!(synced["files"], 2, "{synced}");
    assert_eq!(synced["bytes"], 2 * source.len(), "{synced}");
}
