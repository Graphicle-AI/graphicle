//! The passage index: a question looked up in the index gets the candidates
//! and evidence the full path gives (`GRAPHICLE_PASSAGE_INDEX=0`), also when
//! the relationship lift reaches more passages than `rank` keeps, and then
//! sends `rank` only those it can keep.
use graphicle_runtime::platform::CanonicalExt;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use tempfile::TempDir;

const HUB: &str = "\"\"\"The metric registry.\"\"\"\n\nREGISTRY = {}\n\n\
def hub_metric_register(name, kind=\"gauge\"):\n    \
\"\"\"Register the gauge metric under name in the registry.\"\"\"\n    \
REGISTRY[name] = kind\n    return REGISTRY[name]\n";

const WORDS: &[&str] = &[
    "fold", "window", "batch", "quorum", "ledger", "cursor", "shard", "replica", "lease", "beacon",
    "harbor", "lantern", "meadow", "orchard", "pebble", "quiver", "ripple", "saddle", "thimble",
    "velvet", "wander", "yonder", "zephyr", "anchor", "bramble", "cobalt", "dapple", "ember",
];

/// One of the files that import the hub: 32 functions of about 600
/// characters (about 8 passages), some of which name the question's words.
fn spoke(i: usize) -> String {
    let mut text = format!(
        "from hub import hub_metric_register\n\n\ndef setup_{i}():\n    \
         return hub_metric_register(\"spoke_{i}_total\")\n"
    );
    for j in 0..32 {
        let mut doc = String::new();
        for k in 0..70 {
            doc.push_str(WORDS[(i * 7 + j * 3 + k * 5) % WORDS.len()]);
            doc.push(if k % 10 == 9 { '\n' } else { ' ' });
        }
        if (i + j).is_multiple_of(3) {
            doc.push_str("records the gauge metric\n");
        }
        if (i + j).is_multiple_of(5) {
            doc.push_str("see the registry\n");
        }
        text.push_str(&format!(
            "\n\ndef step_{i}_{j}(values):\n    \"\"\"\n{doc}\"\"\"\n    total = 0\n    \
             for value in values:\n        total += value * {j}\n    return total\n"
        ));
    }
    text
}

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
            let path = this.app.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        this.git(&["add", "-A"]);
        this.git(&["commit", "-q", "-m", "one"]);
        this
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
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// The JSON the program prints in the repository, with `env` set.
    fn json(&self, args: &[&str], env: &[(&str, &str)]) -> Value {
        let mut command = Command::new(env!("CARGO_BIN_EXE_graphicle"));
        crate::child_env::home(&mut command, &self.home)
            .current_dir(&self.app)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_DATA_HOME", self.home.join(".local/share"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GRAPHICLE_DATA_DIR")
            .env_remove("GRAPHICLE_PASSAGE_INDEX")
            .env_remove("GRAPHICLE_DEBUG_RANK")
            .arg("--data-dir")
            .arg(&self.data)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (name, value) in env {
            command.env(name, value);
        }
        let output = crate::fork_guard::spawn(&mut command)
            .unwrap()
            .wait_with_output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
            panic!(
                "{args:?} printed no JSON: {}",
                String::from_utf8_lossy(&output.stdout)
            )
        })
    }
}

/// The `rank` calls a `GRAPHICLE_DEBUG_RANK` file records, in order.
fn ranks(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn listed(packet: &Value, field: &str, key: &str) -> Vec<String> {
    packet[field]
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| item[key].as_str().unwrap_or("").to_string())
        .collect()
}

#[test]
fn a_lift_past_what_rank_keeps_gives_the_full_paths_candidates() {
    let mut files = vec![("hub.py".to_string(), HUB.to_string())];
    for i in 0..150 {
        files.push((format!("spokes/spoke_{i:03}.py"), spoke(i)));
    }
    let h = Home::new(&files);
    let synced = h.json(&["sync", "."], &[]);
    assert!(
        ["built", "current"].contains(&synced["graph"]["status"].as_str().unwrap_or("")),
        "{synced:#}"
    );
    let arguments = h.home.join("question.json");
    std::fs::write(
        &arguments,
        json!({"question":"Where does hub_metric_register register the gauge metric in the registry?",
            "debug_candidates":400})
        .to_string(),
    )
    .unwrap();
    let call = [
        "call",
        "context",
        "--arguments",
        arguments.to_str().unwrap(),
    ];
    let (index_ranks, full_ranks) = (h.home.join("index.jsonl"), h.home.join("full.jsonl"));
    let indexed = h.json(
        &call,
        &[("GRAPHICLE_DEBUG_RANK", index_ranks.to_str().unwrap())],
    );
    let full = h.json(
        &call,
        &[
            ("GRAPHICLE_PASSAGE_INDEX", "0"),
            ("GRAPHICLE_DEBUG_RANK", full_ranks.to_str().unwrap()),
        ],
    );
    assert_eq!(indexed["passage_index"]["status"], "ready", "{indexed:#}");
    assert!(full.get("passage_index").is_none(), "{full:#}");
    // The hub leads, so the lift reaches every other file.
    let candidates = listed(&indexed, "candidates_debug", "identity");
    assert!(
        candidates[..4]
            .iter()
            .any(|id| id.starts_with("file:hub.py:")),
        "{candidates:?}"
    );
    assert_eq!(candidates, listed(&full, "candidates_debug", "identity"));
    assert_eq!(
        listed(&indexed, "evidence", "id"),
        listed(&full, "evidence", "id")
    );
    assert_eq!(indexed["markdown"], full["markdown"]);
    // The full path ranks every passage in one call. The index's first call
    // ranks the arms' passages; its second adds at most MAX_FUSED of the
    // lifted ones, where the lift reaches far more.
    let (index_calls, full_calls) = (ranks(&index_ranks), ranks(&full_ranks));
    assert_eq!((index_calls.len(), full_calls.len()), (2, 1));
    assert!(
        index_calls[1]["edges"].as_u64().unwrap() > 0,
        "{}",
        index_calls[1]
    );
    let rows = |call: &Value| call["rows"].as_u64().unwrap();
    let fused = graphicle_memory::retrieval::MAX_FUSED as u64;
    let (first, second, whole) = (
        rows(&index_calls[0]),
        rows(&index_calls[1]),
        rows(&full_calls[0]),
    );
    assert!(whole > first + fused + 100, "{first} {whole}");
    assert!(second <= first + fused, "{first} {second}");
}

/// Editing one file in sync after sync adds a small segment each time and
/// drops the one the file left empty: the index is merged only when the
/// files that changed hold more than `MAX_DEAD` of all its passages, never
/// for one small segment.
#[test]
fn editing_one_file_again_and_again_never_rewrites_the_index() {
    let files: Vec<(String, String)> = (0..10)
        .map(|i| {
            (
                format!("src/m{i}.py"),
                format!("def f{i}():\n    return {i}\n"),
            )
        })
        .collect();
    let h = Home::new(&files);
    let first = h.json(&["sync", "."], &[]);
    assert_eq!(first["passage_index"]["segments"], 1, "{first:#}");
    for (edits, word) in ["walrus", "narwhal", "axolotl"].iter().enumerate() {
        std::fs::write(
            h.app.join("src/m0.py"),
            format!("def f0():\n    return '{word}'\n"),
        )
        .unwrap();
        let synced = h.json(&["sync", "."], &[]);
        let index = &synced["passage_index"];
        assert_eq!(index["files"], 1, "{synced:#}");
        assert_eq!(index["merged"], false, "{synced:#}");
        // The segment the previous edit wrote holds no current file now.
        assert_eq!(index["emptied"], edits.min(1), "{synced:#}");
    }
    let arguments = h.home.join("edited.json");
    std::fs::write(
        &arguments,
        json!({"question":"Which function returns axolotl?","debug_candidates":20}).to_string(),
    )
    .unwrap();
    let packet = h.json(
        &[
            "call",
            "context",
            "--arguments",
            arguments.to_str().unwrap(),
        ],
        &[],
    );
    assert_eq!(packet["passage_index"]["status"], "ready", "{packet:#}");
    let candidates = listed(&packet, "candidates_debug", "identity");
    assert!(
        candidates
            .first()
            .is_some_and(|id| id.starts_with("file:src/m0.py:")),
        "{candidates:?}"
    );
}
