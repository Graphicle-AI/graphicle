//! `serve --socket` (beyond the upstream port, opt-in): commands answered by the
//! resident server print exactly what they print locally, a request the
//! server declines runs locally, and the socket is private to its user.
#![cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_graphicle-graph"))
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A command with a clean environment: only what the test sets.
fn command(cwd: &Path) -> Command {
    let mut c = Command::new(bin());
    c.current_dir(cwd)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", cwd);
    c
}

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start(cwd: &Path, socket: &Path) -> Server {
    start_with(cwd, socket, &[])
}

/// A server whose own environment adds `env`.
fn start_with(cwd: &Path, socket: &Path, env: &[(&str, &str)]) -> Server {
    let child = command(cwd)
        .envs(env.iter().copied())
        .args(["serve", "--socket"])
        .arg(socket)
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while std::os::unix::net::UnixStream::connect(socket).is_err() {
        assert!(Instant::now() < deadline, "server did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    Server(child)
}

fn same(a: &Output, b: &Output, what: &str) {
    assert_eq!(a.status.code(), b.status.code(), "{what}: exit code");
    assert_eq!(
        String::from_utf8_lossy(&a.stdout),
        String::from_utf8_lossy(&b.stdout),
        "{what}: stdout"
    );
    assert_eq!(
        String::from_utf8_lossy(&a.stderr),
        String::from_utf8_lossy(&b.stderr),
        "{what}: stderr"
    );
}

#[test]
fn socket_answers_match_local_answers() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    write(
        &repo.join("app/models.py"),
        "class Order:\n    def total(self):\n        return price(self)\n\n\ndef price(order):\n    return 1\n",
    );
    write(
        &repo.join("app/views.py"),
        "from app.models import Order, price\n\n\ndef show(order: Order):\n    return price(order)\n",
    );
    let out = command(&repo)
        .args(["extract", ".", "--code-only"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let socket = dir.path().join("graphicle.sock");
    let _server = start(&repo, &socket);
    let mode = std::fs::metadata(&socket).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "socket mode {mode:o}");
    let commands: &[&[&str]] = &[
        &["query", "Order total"],
        &["query", "price", "--dfs", "--budget", "300"],
        &["explain", "Order"],
        &["explain", "no_such_symbol"],
        &["path", "show", "price"],
        &["god-nodes", "--json"],
        &["affected", "price"],
        &["retrieve", "how is the order price computed", "--json"],
        &["query", "price", "--min-evidence", "binding"],
    ];
    for args in commands {
        let local = command(&repo).args(*args).output().unwrap();
        let served = command(&repo)
            .args(*args)
            .env("GRAPHICLE_SOCKET", &socket)
            .output()
            .unwrap();
        same(&local, &served, &args.join(" "));
    }
    // A caller with another engine environment (a GRAPHICLE_ setting the
    // engine reads) is declined and answers locally, with the same output as
    // without the server.
    let args = ["query", "Order total"];
    let local = command(&repo)
        .args(args)
        .env("GRAPHICLE_MAX_GRAPH_BYTES", "8GB")
        .output()
        .unwrap();
    let declined = command(&repo)
        .args(args)
        .env("GRAPHICLE_MAX_GRAPH_BYTES", "8GB")
        .env("GRAPHICLE_SOCKET", &socket)
        .env("GRAPHICLE_SOCKET_DEBUG", "1")
        .output()
        .unwrap();
    assert_eq!(local.stdout, declined.stdout);
    assert!(
        String::from_utf8_lossy(&declined.stderr).contains("ran locally: declined (environment)"),
        "{}",
        String::from_utf8_lossy(&declined.stderr)
    );
    // Only the engine's own variables travel with a request: a setting of
    // another program that shares the GRAPHICLE_ prefix (the runtime's key)
    // is neither sent nor a reason to decline.
    let debug_socket = dir.path().join("debug.sock");
    let _debug_server = start_with(&repo, &debug_socket, &[("GRAPHICLE_SOCKET_DEBUG", "1")]);
    let served = command(&repo)
        .args(args)
        .env("GRAPHICLE_KEY", "not-an-engine-setting")
        .env("GRAPHICLE_SOCKET", &debug_socket)
        .env("GRAPHICLE_SOCKET_DEBUG", "1")
        .output()
        .unwrap();
    assert_eq!(
        command(&repo).args(args).output().unwrap().stdout,
        served.stdout
    );
    assert!(
        !String::from_utf8_lossy(&served.stderr).contains("ran locally"),
        "{}",
        String::from_utf8_lossy(&served.stderr)
    );
    // A socket nobody listens on is simply not used.
    let missing = command(&repo)
        .args(args)
        .env("GRAPHICLE_SOCKET", dir.path().join("absent.sock"))
        .output()
        .unwrap();
    same(
        &command(&repo).args(args).output().unwrap(),
        &missing,
        "absent socket",
    );
    // Copy tools can preserve mtime even when same-size contents change.
    // Both resident caches must notice an in-place rewrite in that case.
    let graph = repo.join(".graphicle/graph.json");
    let meta = std::fs::metadata(&graph).unwrap();
    let old = std::fs::read_to_string(&graph).unwrap();
    let updated = old.replace("price", "costs");
    assert_ne!(old, updated);
    assert_eq!(old.len(), updated.len());
    std::fs::write(&graph, updated).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&graph)
        .unwrap()
        .set_modified(meta.modified().unwrap())
        .unwrap();
    for args in [["query", "price"], ["affected", "price"]] {
        let local = command(&repo).args(args).output().unwrap();
        let served = command(&repo)
            .args(args)
            .env("GRAPHICLE_SOCKET", &socket)
            .output()
            .unwrap();
        same(
            &local,
            &served,
            "same-size graph rewrite with preserved mtime",
        );
    }
}

#[test]
fn serve_socket_refuses_a_path_that_is_not_a_socket() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("taken");
    std::fs::write(&path, "data").unwrap();
    let out = command(dir.path())
        .args(["serve", "--socket"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("is not a socket"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "data");
}
