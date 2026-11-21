//! Read-only mode (`--read-only`, `GRAPHICLE_READ_ONLY=1`): a query leaves
//! every file of the data folder byte for byte as it was, a write is refused,
//! and a folder the mode cannot read faithfully is refused before anything
//! is read.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

/// Every file under `folder`, with its bytes.
fn files(folder: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![folder.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in std::fs::read_dir(&next).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let name = path.strip_prefix(folder).unwrap().display().to_string();
                out.insert(name, std::fs::read(&path).unwrap());
            }
        }
    }
    out
}

fn graphicle(home: &Path, data: &Path, cwd: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_graphicle"));
    crate::child_env::home(&mut command, home);
    command
        .current_dir(cwd)
        .env("GRAPHICLE_NO_UPDATE_NOTIFIER", "1")
        .env_remove("GRAPHICLE_READ_ONLY")
        .arg("--data-dir")
        .arg(data);
    command
}

fn output(mut command: Command) -> Output {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::fork_guard::spawn(&mut command)
        .unwrap()
        .wait_with_output()
        .unwrap()
}

fn ok(command: Command) -> String {
    let out = output(command);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn a_read_only_query_leaves_the_data_folder_byte_for_byte() {
    if graphicle_runtime::db::postgres_configured() {
        eprintln!(
            "skipped: read-only mode opens SQLite data folders; GRAPHICLE_DATABASE_URL is set"
        );
        return;
    }
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let docs = dir.path().join("docs");
    let data = dir.path().join("data");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(
        docs.join("ownership.md"),
        "# Ownership\n\nMoving a String transfers ownership; the old binding is no longer valid.\n",
    )
    .unwrap();
    let mut sync = graphicle(&home, &data, &docs);
    sync.args(["sync", ".", "--json"]);
    let synced: serde_json::Value = serde_json::from_str(&ok(sync)).unwrap();
    let project = synced["repository_id"].as_str().unwrap().to_string();
    let arguments = dir.path().join("args.json");
    std::fs::write(
        &arguments,
        serde_json::json!({"question":"What happens when a String moves?","graph_retrieval":false})
            .to_string(),
    )
    .unwrap();
    let before = files(&data);

    // Both ways of asking answer, through the flag and the environment.
    let mut context = graphicle(&home, &data, &docs);
    context.args(["--read-only", "context", "What happens when a String moves?"]);
    assert!(ok(context).contains("ownership.md"));
    let mut call = graphicle(&home, &data, &docs);
    call.env("GRAPHICLE_READ_ONLY", "1").args([
        "call",
        "context",
        "--project",
        &project,
        "--arguments",
        arguments.to_str().unwrap(),
    ]);
    assert!(ok(call).contains("ownership.md"));
    // A write is refused, and changes nothing either.
    std::fs::write(docs.join("more.md"), "# More\n").unwrap();
    let mut write = graphicle(&home, &data, &docs);
    write.args(["--read-only", "sync", "."]);
    let refused = output(write);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("read-only"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(files(&data) == before, "the data folder changed");

    // Without the mode, a query counts the tokens it served: the store
    // changes, as before.
    let mut counted = graphicle(&home, &data, &docs);
    counted.args([
        "call",
        "context",
        "--project",
        &project,
        "--arguments",
        arguments.to_str().unwrap(),
    ]);
    ok(counted);
    assert!(files(&data) != before, "a counted query changes the store");

    // A data folder last used by another version is refused, untouched.
    let stamp = data.join("version.json");
    std::fs::write(&stamp, br#"{"graphicle":"0.0.1"}"#).unwrap();
    let before = files(&data);
    let mut older = graphicle(&home, &data, &docs);
    older.args(["--read-only", "context", "What happens when a String moves?"]);
    let refused = output(older);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("0.0.1"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(files(&data) == before, "the refused folder changed");
}

/// A data folder on read-only media (here: no write permission anywhere)
/// still answers.
#[cfg(unix)]
#[test]
fn a_read_only_folder_answers_in_read_only_mode() {
    use std::os::unix::fs::PermissionsExt;
    if graphicle_runtime::db::postgres_configured() {
        eprintln!(
            "skipped: read-only mode opens SQLite data folders; GRAPHICLE_DATABASE_URL is set"
        );
        return;
    }
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let docs = dir.path().join("docs");
    let data = dir.path().join("data");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(
        docs.join("borrow.md"),
        "# Borrowing\n\nA reference borrows a value.\n",
    )
    .unwrap();
    let mut sync = graphicle(&home, &data, &docs);
    sync.args(["sync", ".", "--json"]);
    ok(sync);
    let lock = |mode: u32| {
        let mut stack = vec![data.clone()];
        let mut all = vec![];
        while let Some(next) = stack.pop() {
            for entry in std::fs::read_dir(&next).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path.clone());
                }
                all.push(path);
            }
        }
        all.push(data.clone());
        for path in all {
            let mut permissions = std::fs::metadata(&path).unwrap().permissions();
            let current = permissions.mode();
            permissions.set_mode(if mode == 0 {
                current & !0o222
            } else {
                current | 0o200
            });
            std::fs::set_permissions(&path, permissions).unwrap();
        }
    };
    lock(0);
    let before = files(&data);
    let mut context = graphicle(&home, &data, &docs);
    context.args(["--read-only", "context", "What does a reference do?"]);
    let answered = output(context);
    lock(1);
    assert!(
        answered.status.success(),
        "{}",
        String::from_utf8_lossy(&answered.stderr)
    );
    assert!(String::from_utf8_lossy(&answered.stdout).contains("borrow.md"));
    assert!(files(&data) == before, "the data folder changed");
}

/// A code project whose saved query files are missing (as after an upgrade,
/// whose new build writes its own) still packs in read-only mode, from the
/// graph in the store, and no file appears; a writing command outside the
/// store (`config set`) is refused.
#[test]
fn a_read_only_pack_writes_no_query_file() {
    if graphicle_runtime::db::postgres_configured() {
        eprintln!(
            "skipped: read-only mode opens SQLite data folders; GRAPHICLE_DATABASE_URL is set"
        );
        return;
    }
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let code = dir.path().join("code");
    let data = dir.path().join("data");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&code).unwrap();
    std::fs::write(
        code.join("prices.py"),
        "def price_with_tax(price):\n    return round(price * 1.2, 2)\n",
    )
    .unwrap();
    let mut sync = graphicle(&home, &data, &code);
    sync.args(["sync", ".", "--json"]);
    ok(sync);
    // Every saved query file goes, as an upgrade leaves them for another
    // build.
    let mut removed = 0;
    let mut stack = vec![data.join("gf-cache")];
    while let Some(next) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.parent().and_then(Path::file_name) == Some("query".as_ref()) {
                std::fs::remove_file(&path).unwrap();
                removed += 1;
            }
        }
    }
    assert!(removed > 0, "the sync saved no query file");
    let before = files(&data);
    let mut pack = graphicle(&home, &data, &code);
    pack.args(["--read-only", "pack", "where is tax added to prices?"]);
    assert!(ok(pack).contains("price_with_tax"));
    let mut set = graphicle(&home, &data, &code);
    set.args(["--read-only", "config", "set", "model", "ollama"]);
    let refused = output(set);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("read-only"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(files(&data) == before, "the data folder changed");
}
