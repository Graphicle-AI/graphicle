//! Fault isolation in the shared store: reads use read-only snapshots beside
//! a held write lock, a graph projection builds without holding the lock,
//! and slow write transactions are reported with their call site.
use graphicle_runtime::store::{self, Store, s};
use serde_json::{Value, json};
use std::path::Path;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const A: &str = "def alpha():\n    return 1\n\n\nclass Tank:\n    def fill(self, litres):\n        return alpha() + litres\n";
const B: &str = "from a import alpha, Tank\n\n\ndef run():\n    return Tank().fill(alpha())\n";

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
        let ws = store.create_workspace("Isolation").unwrap();
        let (workspace, key) = (s(&ws, "id").to_string(), s(&ws, "capability").to_string());
        let registered = store
            .register(
                &workspace,
                &key,
                &json!({"project_key":"isolation","name":"Isolation"}),
            )
            .unwrap();
        let repo = s(&registered, "id").to_string();
        let f = Self {
            _dir: dir,
            store,
            workspace,
            key,
            repo,
        };
        let status = f
            .store
            .project_status(&f.workspace, &f.key, &f.args(json!({})))
            .unwrap();
        let manifest: Vec<Value> = files
            .iter()
            .map(|(name, text)| json!({"path":name,"sha256":graphicle_memory::text::sha256(text.as_bytes()),"bytes":text.len()}))
            .collect();
        let begin = f
            .store
            .sync_prepare(&f.workspace, &f.key, &f.args(json!({"expected_snapshot":status["repository"]["snapshot"],"manifest":manifest})))
            .unwrap();
        let uploads: Vec<Value> = files
            .iter()
            .map(|(name, text)| json!({"path":name,"content":text}))
            .collect();
        f.store
            .sync_upload(
                &f.workspace,
                &f.key,
                &f.args(json!({"sync_id":begin["sync_id"],"files":uploads})),
            )
            .unwrap();
        f.store
            .sync_commit(
                &f.workspace,
                &f.key,
                &f.args(json!({"sync_id":begin["sync_id"]})),
            )
            .unwrap();
        f
    }
    fn args(&self, mut args: Value) -> Value {
        args["repository_id"] = json!(self.repo);
        args
    }
}

fn has_file(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| {
            let path = entry.path();
            if path.is_dir() { has_file(&path) } else { true }
        })
}

#[test]
fn reads_run_beside_a_held_write_lock_and_cannot_write() {
    let f = Fixture::new(&[("a.py", A)]);
    let writer = f.store.connect().unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    let started = Instant::now();
    let status = f
        .store
        .project_status(&f.workspace, &f.key, &f.args(json!({})))
        .unwrap();
    assert_eq!(status["files"], 1, "{status}");
    let files = f
        .store
        .project_read(&f.workspace, &f.key, "files", &f.args(json!({})))
        .unwrap();
    assert_eq!(files["total"], 1);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "a read waited for the writer"
    );
    // A write inside a read snapshot fails at once instead of upgrading.
    let error = f
        .store
        .read(|db| {
            db.execute("DELETE FROM repo_events", [])?;
            Ok(())
        })
        .unwrap_err();
    assert!(
        error.message.to_lowercase().contains("readonly"),
        "{}",
        error.message
    );
    writer.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn a_projection_builds_while_another_writer_holds_the_lock() {
    let f = Fixture::new(&[("a.py", A), ("b.py", B)]);
    let cache = f.store.directory.join("gf-cache").join(&f.repo);
    let writer = f.store.connect().unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    let (store, workspace, key, repo) = (
        f.store.clone(),
        f.workspace.clone(),
        f.key.clone(),
        f.repo.clone(),
    );
    let projection = std::thread::spawn(move || store.graph_project(&workspace, &key, &repo));
    // The extraction cache fills while this connection still holds the write
    // lock: the build no longer waits for it. Only the short commit does.
    let deadline = Instant::now() + Duration::from_secs(8);
    while !has_file(&cache) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    let built_under_lock = has_file(&cache);
    writer.execute_batch("ROLLBACK").unwrap();
    let info = projection.join().unwrap().unwrap();
    assert!(
        built_under_lock,
        "the graph build waited for another writer's lock"
    );
    assert!(info["nodes"].as_u64().unwrap() > 0, "{info}");
    // A second request finds the recorded episode.
    let again = f
        .store
        .graph_project(&f.workspace, &f.key, &f.repo)
        .unwrap();
    assert_eq!(again["replayed"], true, "{again}");
}

#[test]
fn a_slow_write_transaction_is_reported_with_its_call_site() {
    let f = Fixture::new(&[("a.py", A)]);
    f.store
        .transaction(|_| {
            std::thread::sleep(Duration::from_millis(2100));
            Ok(())
        })
        .unwrap();
    let report = store::lock_report();
    assert!(report["slow_holds"].as_u64().unwrap() >= 1, "{report}");
    assert!(
        report["recent_slow"]
            .as_array()
            .unwrap()
            .iter()
            .any(|hold| s(hold, "at").contains("fault_isolation.rs")),
        "{report}"
    );
    let status = f
        .store
        .project_status(&f.workspace, &f.key, &f.args(json!({})))
        .unwrap();
    assert!(status["write_lock"]["write_transactions"].as_u64().unwrap() > 0);
}
