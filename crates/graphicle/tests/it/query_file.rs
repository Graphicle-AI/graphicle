//! Saved query files (`graphicle_runtime::query_file`): `graphicle context`
//! reads the code graph's query file, for the retrieval's arms and for the
//! identifier route, and prints the packet it prints when it reads the saved
//! graph instead (`GRAPHICLE_QUERY_FILE=0`), byte for byte. A damaged or
//! truncated query file, saved graph or file relationships file is found,
//! read around with the same packet, and written again by the next sync; a
//! file that cannot be written is tried again after a wait, or at once by
//! `graphicle sync`; and switching back to an earlier head set finds its
//! files.
use graphicle_runtime::platform::CanonicalExt;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

const STORE: &str = "\"\"\"The record store.\"\"\"\n\n\
class Store:\n    \"\"\"Keeps records in a write ahead log.\"\"\"\n\n    \
def open(self, path):\n        \"\"\"Open the database at path.\"\"\"\n        \
self.path = path\n        return self\n\n    \
def close(self):\n        \"\"\"Close the log and flush it.\"\"\"\n        \
self.path = None\n";

const QUERY: &str = "from store import Store\n\n\n\
def run_query(path, text):\n    \"\"\"Run a query against the store at path.\"\"\"\n    \
store = Store().open(path)\n    rows = plan(text)\n    store.close()\n    return rows\n\n\n\
def plan(text):\n    \"\"\"Split a query into its terms.\"\"\"\n    \
return text.split()\n";

const README: &str = "# Fixture\n\nThe [store](store.py) keeps records; `run_query` reads them.\n";

fn helper(i: usize) -> String {
    format!(
        "from query import run_query\n\n\n\
         def report_{i}(path):\n    \"\"\"Report {i} counts the rows of a query.\"\"\"\n    \
         return len(run_query(path, \"select {i}\"))\n\n\n\
         def audit_{i}(path):\n    \"\"\"Audit {i} reopens the store.\"\"\"\n    \
         return report_{i}(path) + {i}\n"
    )
}

/// Questions for the retrieval's arms, then lookups the identifier route
/// answers.
const QUESTIONS: [&str; 4] = [
    "Where does Store.open open the database, and who calls it?",
    "Bug report: audit_3 reopens the store without closing the write ahead log. Where is that?",
    "How does plan split a query into its terms?",
    "Store",
];
const LOOKUPS: [&str; 5] = [
    "Where is run_query defined?",
    "run_query",
    "who calls Store.open",
    "Where are report_3 and audit_7 defined?",
    "Which functions call run_query? List every calling file.",
];

struct Home {
    _dir: TempDir,
    home: PathBuf,
    data: PathBuf,
    app: PathBuf,
}

impl Home {
    fn new(files: &[(String, String)]) -> Self {
        let dir = TempDir::new().unwrap();
        let home = dir.path().canonical().unwrap();
        let app = home.join("app");
        std::fs::create_dir_all(&app).unwrap();
        let this = Self {
            data: home.join("data"),
            app,
            home,
            _dir: dir,
        };
        this.git(&["init", "-q", "-b", "main"]);
        for (path, text) in files {
            this.write(path, text);
        }
        this.git(&["add", "-A"]);
        this.git(&["commit", "-q", "-m", "one"]);
        this
    }

    fn write(&self, path: &str, text: &str) {
        let path = self.app.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn git(&self, args: &[&str]) {
        let mut command = Command::new("git");
        crate::child_env::home(&mut command, &self.home)
            .arg("-C")
            .arg(&self.app)
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = crate::fork_guard::spawn(&mut command)
            .unwrap()
            .wait_with_output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}");
    }

    /// What the program prints in the repository (standard output, then
    /// standard error), with `env` set and `input` on its standard input.
    fn run(&self, args: &[&str], env: &[(&str, &str)], input: &str) -> (Vec<u8>, String) {
        use std::io::Write;
        let mut command = Command::new(env!("CARGO_BIN_EXE_graphicle"));
        crate::child_env::home(&mut command, &self.home)
            .current_dir(&self.app)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_DATA_HOME", self.home.join(".local/share"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GRAPHICLE_DATA_DIR")
            .env_remove("GRAPHICLE_PASSAGE_INDEX")
            .env_remove("GRAPHICLE_QUERY_FILE")
            .env_remove("GRAPHICLE_SAVED_GRAPH")
            .env_remove("GRAPHICLE_PHASE_TIMINGS")
            .arg("--data-dir")
            .arg(&self.data)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (name, value) in env {
            command.env(name, value);
        }
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
        (output.stdout, stderr)
    }

    fn json(&self, args: &[&str]) -> Value {
        serde_json::from_slice(&self.run(args, &[], "").0).unwrap()
    }

    /// What the prompt hook prints for `question`, with `env` set and the
    /// longest budget; `session` names the session, so that a note for the
    /// person is never held back from one call because another gave it.
    fn hook(&self, question: &str, env: &[(&str, &str)], session: &str) -> Vec<u8> {
        let payload = serde_json::json!({"session_id": session,
            "cwd": self.app.to_string_lossy(),
            "hook_event_name": "UserPromptSubmit", "prompt": question});
        let mut env = env.to_vec();
        env.push(("GRAPHICLE_PROMPT_CONTEXT_SECONDS", "25"));
        self.run(
            &["hook", "claude-code", "prompt-context"],
            &env,
            &payload.to_string(),
        )
        .0
    }

    /// `graphicle doctor`'s report, whatever its exit status.
    fn doctor(&self) -> Value {
        let mut command = Command::new(env!("CARGO_BIN_EXE_graphicle"));
        crate::child_env::home(&mut command, &self.home)
            .current_dir(&self.app)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_DATA_HOME", self.home.join(".local/share"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GRAPHICLE_DATA_DIR")
            .arg("--data-dir")
            .arg(&self.data)
            .args(["doctor", "--json"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = crate::fork_guard::spawn(&mut command)
            .unwrap()
            .wait_with_output()
            .unwrap();
        serde_json::from_slice(&output.stdout).unwrap()
    }

    /// The project's query folder.
    fn folder(&self) -> PathBuf {
        let cache = self.data.join("gf-cache");
        std::fs::read_dir(&cache)
            .unwrap()
            .flatten()
            .map(|project| project.path().join("query"))
            .find(|folder| folder.is_dir())
            .unwrap()
    }

    /// The files of the query folder with `extension`.
    fn files(&self, extension: &str) -> Vec<PathBuf> {
        let mut found: Vec<PathBuf> = std::fs::read_dir(self.folder())
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|e| e == extension))
            .collect();
        found.sort();
        found
    }

    /// The packet of `question` and the phases that ran, with the query
    /// file and without it.
    fn both(&self, question: &str) -> ((Vec<u8>, String), (Vec<u8>, String)) {
        let timed = ("GRAPHICLE_PHASE_TIMINGS", "1");
        (
            self.run(&["context", "--json", "-"], &[timed], question),
            self.run(
                &["context", "--json", "-"],
                &[timed, ("GRAPHICLE_QUERY_FILE", "0")],
                question,
            ),
        )
    }
}

fn phases(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .filter_map(|line| line.strip_prefix("phase="))
        .filter_map(|line| line.split_whitespace().next())
        .collect()
}

/// What the query file's readers noted (`phase=<arm>_query_file_read
/// pages=N of=M corrupt=0|1`): the arm, the pages read and whether a read
/// found the file corrupt.
fn reads(stderr: &str) -> Vec<(String, usize, bool)> {
    stderr
        .lines()
        .filter_map(|line| line.strip_prefix("phase="))
        .filter(|line| {
            line.split_whitespace()
                .next()
                .is_some_and(|name| name.ends_with("_query_file_read"))
        })
        .map(|line| {
            let field = |key: &str| {
                line.split_whitespace()
                    .find_map(|part| part.strip_prefix(key))
                    .unwrap_or("")
                    .to_string()
            };
            (
                line.split_whitespace().next().unwrap_or("").to_string(),
                field("pages=").parse().unwrap_or(0),
                field("corrupt=") == "1",
            )
        })
        .collect()
}

/// Every question prints the same packet with and without the query file,
/// and the file answered: its reader noted the pages it read and found it
/// whole, and the saved graph was not read. Without the file, nothing reads
/// it and the saved graph answers.
fn same_packets(h: &Home, questions: &[&str], route: bool) {
    for question in questions {
        let ((read, read_phases), (saved, saved_phases)) = h.both(question);
        let packet: Value = serde_json::from_slice(&read).unwrap();
        assert!(
            packet["markdown"].as_str().is_some_and(|m| !m.is_empty()),
            "{question}: {packet:#}"
        );
        assert_eq!(
            String::from_utf8_lossy(&read),
            String::from_utf8_lossy(&saved),
            "{question}"
        );
        let marker = if route {
            "route_query_file_read"
        } else {
            "arm_query_file_read"
        };
        let noted = reads(&read_phases);
        assert!(
            noted
                .iter()
                .any(|(name, pages, corrupt)| name == marker && *pages > 0 && !corrupt),
            "{question}: {noted:?}\n{read_phases}"
        );
        assert!(
            noted.iter().all(|(_, _, corrupt)| !corrupt),
            "{question}: {noted:?}"
        );
        assert!(
            reads(&saved_phases).is_empty(),
            "{question}: {saved_phases}"
        );
        let (read_phases, saved_phases) = (phases(&read_phases), phases(&saved_phases));
        assert!(
            !read_phases.contains(&"graph_saved_read"),
            "{question}: {read_phases:?}"
        );
        assert!(
            saved_phases.contains(&"graph_saved_read"),
            "{question}: {saved_phases:?}"
        );
        assert_eq!(
            packet["route"]["path"] == "identifier",
            route,
            "{question}: {packet:#}"
        );
    }
}

fn fixture() -> Home {
    let mut files = vec![
        ("store.py".to_string(), STORE.to_string()),
        ("query.py".to_string(), QUERY.to_string()),
        ("README.md".to_string(), README.to_string()),
    ];
    for i in 0..12 {
        files.push((format!("reports/report_{i}.py"), helper(i)));
    }
    Home::new(&files)
}

#[test]
fn context_and_the_identifier_route_read_the_query_file_and_print_the_saved_graphs_packet() {
    let h = fixture();
    let synced = h.json(&["sync", "."]);
    assert_eq!(
        synced["graph"]["saved_graph"]["query_file"], "written",
        "{synced:#}"
    );
    let written = h.files("query");
    assert_eq!(written.len(), 1, "{written:?}");
    same_packets(&h, &QUESTIONS, false);
    same_packets(&h, &LOOKUPS, true);
    // The prompt hook prints the same output with and without the file, for
    // prompts it answers: ones that name code (the hook leaves others alone).
    for (k, question) in [
        QUESTIONS[1],
        "How does `plan` split a query into its terms?",
        LOOKUPS[0],
    ]
    .into_iter()
    .enumerate()
    {
        let with = h.hook(question, &[], &format!("file-{k}"));
        let without = h.hook(
            question,
            &[("GRAPHICLE_QUERY_FILE", "0")],
            &format!("saved-{k}"),
        );
        let printed: Value = serde_json::from_slice(&with).unwrap();
        assert!(
            printed["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .is_some_and(|markdown| !markdown.is_empty()),
            "{question}: {printed:#}"
        );
        assert_eq!(
            String::from_utf8_lossy(&with),
            String::from_utf8_lossy(&without),
            "{question}"
        );
    }
    // A store an earlier version synced: the graph is saved, the query file
    // is not. The next sync writes it from the saved graph.
    std::fs::remove_file(&written[0]).unwrap();
    let again = h.json(&["sync", "."]);
    assert_eq!(
        again["graph"]["saved_graph"]["query_file"], "written",
        "{again:#}"
    );
    assert_eq!(h.files("query"), written);
    // Nothing changed: the next sync keeps it.
    let kept = h.json(&["sync", "."]);
    assert_eq!(kept["graph"]["saved_graph"]["status"], "saved", "{kept:#}");
    same_packets(&h, &QUESTIONS[..2], false);
    assert!(Path::new(&written[0]).is_file());
}

/// The bytes of section `s` of a query file: (offset, length), from its
/// header (magic, key length and key, then 16 bytes a section).
fn section(bytes: &[u8], s: usize) -> (usize, usize) {
    let length = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    let at = 12 + length + s * 16;
    let read = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap()) as usize;
    (read(at), read(at + 8))
}

/// A flipped byte in the postings, in the code symbols or in the node ids is
/// found by the question that reads it (the page checksums find it), and so
/// is a bad entry in the node ids whose checksums were written again (the
/// reader's own checks find it): the packet is the saved graph's, the file
/// is removed, and the next sync writes it again.
#[test]
fn a_damaged_query_file_is_read_around_and_written_again() {
    use graphicle_runtime::query_file::{IDS_SECTION, POSTINGS_SECTION, SYMBOLS_SECTION};
    /// A byte flipped in every page of a section, or one entry of the node
    /// ids in order naming no node, with whole checksums.
    enum Damage {
        Flipped(usize),
        BadId,
    }
    let arm = "Where does Store.open open the database, and who calls it?";
    let cases = [
        (
            Damage::Flipped(POSTINGS_SECTION),
            "How does plan split a query into its terms?",
        ),
        (
            Damage::Flipped(SYMBOLS_SECTION),
            "Where is run_query defined?",
        ),
        (Damage::Flipped(IDS_SECTION), arm),
        (Damage::BadId, arm),
    ];
    let h = fixture();
    h.json(&["sync", "."]);
    let path = h.files("query").remove(0);
    let pristine = std::fs::read(&path).unwrap();
    for (k, (damage, question)) in cases.into_iter().enumerate() {
        let mut bytes = pristine.clone();
        match damage {
            Damage::Flipped(s) => {
                let (offset, len) = section(&pristine, s);
                assert!(len > 0, "section {s}");
                let mut at = offset;
                while at < offset + len {
                    bytes[at] ^= 0x5a;
                    at += 4096;
                }
            }
            Damage::BadId => {
                // The node count (the third u32 of the first section), and
                // the entry a lookup reads first: the middle one.
                let meta = section(&pristine, 0).0;
                let n = u32::from_le_bytes(bytes[meta + 8..meta + 12].try_into().unwrap());
                let at = section(&pristine, IDS_SECTION).0 + (n as usize / 2) * 4;
                bytes[at..at + 4].copy_from_slice(&n.to_le_bytes());
                assert!(graphicle_runtime::query_file::reseal(&mut bytes));
            }
        }
        std::fs::write(&path, &bytes).unwrap();
        let ((read, read_phases), (saved, _)) = h.both(question);
        assert_eq!(
            String::from_utf8_lossy(&read),
            String::from_utf8_lossy(&saved),
            "case {k}: {question}"
        );
        assert!(
            !path.exists(),
            "case {k}: the damage was not found: {read_phases}"
        );
        assert!(
            reads(&read_phases).iter().any(|(_, _, corrupt)| *corrupt),
            "case {k}: {read_phases}"
        );
        let synced = h.json(&["sync", "."]);
        assert_eq!(
            synced["graph"]["saved_graph"]["query_file"], "written",
            "case {k}: {synced:#}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), pristine, "case {k}");
    }
}

/// The record of a failed write (`<episode>.query-failed`): its state line
/// (`retry <seconds> <attempts>` or `permanent`), and the record with that
/// line replaced by `state`.
fn failure_state(record: &[u8], state: Option<&str>) -> (String, Vec<u8>) {
    let at = record.iter().position(|&b| b == b'\n').unwrap() + 1;
    let end = at + record[at..].iter().position(|&b| b == b'\n').unwrap();
    let line = String::from_utf8_lossy(&record[at..end]).into_owned();
    let mut replaced = record[..at].to_vec();
    replaced.extend_from_slice(state.unwrap_or(&line).as_bytes());
    replaced.extend_from_slice(&record[end..]);
    (line, replaced)
}

/// A query file that cannot be written for a reason that can pass (here a
/// folder in its place, as a full disk would do) is recorded with the time
/// syncs try again: until then a sync nobody asked for (the background
/// build) does not write it again, while `graphicle sync` tries again at
/// once, and `doctor` says what to do. Once the cause is gone, the first
/// try after the wait, or `graphicle sync` at once, writes it.
#[test]
fn a_failed_write_is_tried_again_after_its_wait_or_at_once_by_graphicle_sync() {
    let h = fixture();
    let synced = h.json(&["sync", "."]);
    let project = synced["repository_id"].as_str().unwrap().to_string();
    let path = h.files("query").remove(0);
    let pristine = std::fs::read(&path).unwrap();
    let note =
        |report: &Value| -> String { report["query_file"].as_str().unwrap_or("").to_string() };
    let background =
        || h.json(&["build-graph", "--project", project.as_str()])["saved_graph"].clone();
    let sync = || h.json(&["sync", "."])["graph"]["saved_graph"].clone();
    std::fs::remove_file(&path).unwrap();
    // A folder in the file's place: the write fails.
    std::fs::create_dir(&path).unwrap();
    let failed = sync();
    assert!(note(&failed).starts_with("error: "), "{failed:#}");
    let record = h.files("query-failed").remove(0);
    let (state, _) = failure_state(&std::fs::read(&record).unwrap(), None);
    assert!(
        state.starts_with("retry ") && state.ends_with(" 1"),
        "{state}"
    );
    // The background build waits: nothing is tried.
    let waiting = background();
    assert_eq!(waiting["status"], "saved", "{waiting:#}");
    assert!(
        note(&waiting).starts_with("not written: ")
            && note(&waiting).contains("graphicle sync at once"),
        "{waiting:#}"
    );
    // `graphicle sync` tries again at once (and fails again: the folder is
    // still there), and the wait doubles.
    let again = sync();
    assert!(note(&again).starts_with("error: "), "{again:#}");
    let (state, _) = failure_state(&std::fs::read(&record).unwrap(), None);
    assert!(state.ends_with(" 2"), "{state}");
    let doctor = h.doctor();
    let questions: Vec<&Value> = doctor["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|check| check["id"] == "context")
        .collect();
    assert!(
        questions.iter().any(|check| {
            let text = check.to_string();
            check["state"] == "warn"
                && text.contains("could not be written")
                && text.contains("free disk space")
                && text.contains("graphicle sync")
        }),
        "{doctor:#}"
    );
    // Questions still print the saved graph's packet.
    let ((read, _), (saved, _)) = h.both(QUESTIONS[0]);
    assert_eq!(read, saved);
    // The cause goes; the background build still waits.
    std::fs::remove_dir(&path).unwrap();
    let waiting = background();
    assert!(note(&waiting).starts_with("not written: "), "{waiting:#}");
    assert!(!path.exists());
    // The wait is over: the background build tries again and writes it.
    let (_, past) = failure_state(&std::fs::read(&record).unwrap(), Some("retry 0 2"));
    std::fs::write(&record, past).unwrap();
    let retried = background();
    assert_eq!(note(&retried), "written", "{retried:#}");
    assert_eq!(std::fs::read(&path).unwrap(), pristine);
    assert!(h.files("query-failed").is_empty());
    // `graphicle sync` right after the cause goes writes it at once.
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(note(&sync()).starts_with("error: "));
    std::fs::remove_dir(&path).unwrap();
    let fixed = sync();
    assert_eq!(note(&fixed), "written", "{fixed:#}");
    assert_eq!(std::fs::read(&path).unwrap(), pristine);
    same_packets(&h, &QUESTIONS[..1], false);
}

/// A query file cut short (in its body or right after its key), or with a
/// changed byte in its section table, its page checksums or its header's
/// own checksum, is not current: the next sync writes it again, byte for
/// byte, and a question before that removes it and prints the saved
/// graph's packet.
#[test]
fn a_query_file_with_a_damaged_header_is_written_again() {
    let h = fixture();
    h.json(&["sync", "."]);
    let path = h.files("query").remove(0);
    let pristine = std::fs::read(&path).unwrap();
    let question = QUESTIONS[0];
    let ((expected, _), _) = h.both(question);
    let key_len = u32::from_le_bytes(pristine[8..12].try_into().unwrap()) as usize;
    let table = 12 + key_len;
    let after = table + 19 * 16;
    let body = u64::from_le_bytes(pristine[after..after + 8].try_into().unwrap()) as usize;
    let flipped = |at: usize| {
        let mut bytes = pristine.clone();
        bytes[at] ^= 0x5a;
        bytes
    };
    let cases = [
        (
            "cut 100 bytes short",
            pristine[..pristine.len() - 100].to_vec(),
            true,
        ),
        ("cut after the key", pristine[..table + 3].to_vec(), false),
        ("section table", flipped(table + 16 + 2), true),
        ("page checksums", flipped(after + 16 + 3), false),
        ("header checksum", flipped(body - 8), true),
    ];
    for (what, damaged, ask_first) in cases {
        std::fs::write(&path, &damaged).unwrap();
        if ask_first {
            let ((read, phases), (saved, _)) = h.both(question);
            assert_eq!(read, expected, "{what}");
            assert_eq!(saved, expected, "{what}");
            assert!(!path.exists(), "{what}: not removed: {phases}");
        }
        let synced = h.json(&["sync", "."]);
        assert_eq!(
            synced["graph"]["saved_graph"]["query_file"], "written",
            "{what}: {synced:#}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), pristine, "{what}");
    }
    same_packets(&h, &QUESTIONS[..1], false);
}

/// A varint of a file relationships file at `at`, which it moves past it.
fn varint(bytes: &[u8], at: &mut usize) -> u64 {
    let (mut value, mut shift) = (0u64, 0);
    loop {
        let byte = bytes[*at];
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return value;
        }
        shift += 7;
    }
}

/// Where the pairs of a file relationships file
/// (`graphicle_runtime::query_graph::FileEdges`) hold their source file,
/// for those one byte long, and how many files its table holds.
fn pair_sources(bytes: &[u8]) -> (Vec<usize>, u64) {
    let mut at = 0usize;
    let key = varint(bytes, &mut at) as usize;
    at += key;
    varint(bytes, &mut at);
    let mut files = 0;
    for table in 0..2 {
        let count = varint(bytes, &mut at);
        if table == 0 {
            files = count;
        }
        for _ in 0..count {
            let n = varint(bytes, &mut at) as usize;
            at += n;
        }
    }
    let pairs = varint(bytes, &mut at);
    let mut found = Vec::new();
    for _ in 0..pairs {
        let start = at;
        varint(bytes, &mut at);
        if at == start + 1 {
            found.push(start);
        }
        for _ in 0..3 {
            varint(bytes, &mut at);
        }
    }
    (found, files)
}

/// A saved graph or a file relationships file cut short or with a changed
/// byte (their trailers give their length and checksum), and a file
/// relationships file whose pairs name files outside its table with its
/// trailer written again (only the decoder's own checks find that): each
/// is read around with the same packet, never a crash, the read that finds
/// it removes it, and the next sync writes it again.
#[test]
fn a_damaged_saved_graph_or_file_relationships_file_is_written_again() {
    let h = fixture();
    h.json(&["sync", "."]);
    let graph = h.files("graph").remove(0);
    let edges = h.files("files").remove(0);
    let pristine_graph = std::fs::read(&graph).unwrap();
    let pristine_edges = std::fs::read(&edges).unwrap();
    let question = QUESTIONS[0];
    let ask = |env: &[(&str, &str)]| h.run(&["context", "--json", "-"], env, question).0;
    let expected = ask(&[]);
    // The saved graph is read when questions do not read the query file.
    let saved_graph = [("GRAPHICLE_QUERY_FILE", "0")];
    assert_eq!(ask(&saved_graph), expected);
    let mut changed = pristine_graph.clone();
    let middle = changed.len() / 2;
    changed[middle] ^= 0x5a;
    std::fs::write(&graph, &changed).unwrap();
    assert_eq!(ask(&saved_graph), expected, "saved graph: a changed byte");
    assert!(!graph.exists(), "saved graph: a changed byte was not found");
    let synced = h.json(&["sync", "."]);
    assert_eq!(
        synced["graph"]["saved_graph"]["status"], "written",
        "{synced:#}"
    );
    assert_eq!(std::fs::read(&graph).unwrap(), pristine_graph);
    // Cut short: the next sync alone finds it and writes it again.
    std::fs::write(&graph, &pristine_graph[..pristine_graph.len() - 100]).unwrap();
    let synced = h.json(&["sync", "."]);
    assert_eq!(
        synced["graph"]["saved_graph"]["status"], "written",
        "{synced:#}"
    );
    assert_eq!(std::fs::read(&graph).unwrap(), pristine_graph);
    // The file relationships: a changed byte, then pairs out of the table.
    let content = &pristine_edges[..pristine_edges.len() - 16];
    let (sources, files) = pair_sources(content);
    assert!(
        !sources.is_empty() && files < 64,
        "{} {files}",
        sources.len()
    );
    let mut changed = pristine_edges.clone();
    changed[sources[0]] ^= 0x01;
    let mut outside = pristine_edges.clone();
    for &at in &sources {
        outside[at] |= 0x40;
    }
    graphicle_runtime::query_graph::reseal(&mut outside);
    for (what, damaged) in [
        ("a changed byte", changed),
        ("pairs out of the table", outside),
    ] {
        std::fs::write(&edges, &damaged).unwrap();
        assert_eq!(ask(&[]), expected, "file relationships: {what}");
        assert!(!edges.exists(), "file relationships: {what} was not found");
        let synced = h.json(&["sync", "."]);
        assert_eq!(
            synced["graph"]["saved_graph"]["status"], "written",
            "{what}: {synced:#}"
        );
        assert_eq!(std::fs::read(&edges).unwrap(), pristine_edges, "{what}");
    }
}

/// An interrupted write's temporary file goes once it is stale; switching
/// back to an earlier head set finds that episode's files kept.
#[test]
fn stale_temporary_files_go_and_an_earlier_episode_keeps_its_files() {
    let h = fixture();
    h.json(&["sync", "."]);
    let folder = h.folder();
    let first = h.files("query");
    // A temporary file an interrupted write left an hour and more ago, and
    // one a write in progress holds.
    let stale = folder.join(".tmpStale0");
    let fresh = folder.join(".tmpFresh0");
    std::fs::write(&stale, b"partial").unwrap();
    std::fs::write(&fresh, b"partial").unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
    std::fs::File::options()
        .write(true)
        .open(&stale)
        .unwrap()
        .set_modified(old)
        .unwrap();
    // Another head set: its own files, and the first episode's are kept.
    h.write(
        "query.py",
        &format!("{QUERY}\n\ndef more(text):\n    return plan(text)\n"),
    );
    let second = h.json(&["sync", "."]);
    assert_eq!(
        second["graph"]["saved_graph"]["status"], "written",
        "{second:#}"
    );
    assert!(!stale.exists());
    assert!(fresh.exists());
    assert_eq!(h.files("query").len(), 2);
    // Back to the first head set: its files are there, read as they are.
    h.write("query.py", QUERY);
    let back = h.json(&["sync", "."]);
    assert_eq!(back["graph"]["saved_graph"]["status"], "saved", "{back:#}");
    assert!(h.files("query").contains(&first[0]));
    same_packets(&h, &QUESTIONS[..1], false);
    // A third head set: the first was synced last, so its files are the
    // ones kept beside the third's (the second's go, though they were
    // written after the first's), and going back to the first reads them.
    h.write(
        "query.py",
        &format!("{QUERY}\n\ndef other(text):\n    return text\n"),
    );
    let third = h.json(&["sync", "."]);
    assert_eq!(
        third["graph"]["saved_graph"]["status"], "written",
        "{third:#}"
    );
    let kept = h.files("query");
    assert_eq!(kept.len(), 2, "{kept:?}");
    assert!(kept.contains(&first[0]), "{kept:?}");
    h.write("query.py", QUERY);
    let again = h.json(&["sync", "."]);
    assert_eq!(
        again["graph"]["saved_graph"]["status"], "saved",
        "{again:#}"
    );
    same_packets(&h, &QUESTIONS[..1], false);
}
