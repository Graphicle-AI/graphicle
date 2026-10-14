//! `watch` refuses its unsupported options, `--incremental` and `--poll-ms`
//! (unit `engine-watch-incremental`), at once. As unknown options they took the next word as their value, so
//! `watch --incremental ./proj` silently watched `.`.
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn watch_refuses_its_unsupported_options_instead_of_watching() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("proj");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("app.py"), "def hello():\n    return 1\n").unwrap();
    let path = project.to_str().unwrap();
    for (args, option) in [
        (vec!["watch", "--incremental", path], "--incremental"),
        (vec!["watch", path, "--incremental"], "--incremental"),
        (vec!["watch", "--incremental=1", path], "--incremental"),
        (vec!["watch", "--poll-ms", "50", path], "--poll-ms"),
        (vec!["watch", path, "--poll-ms=50"], "--poll-ms"),
    ] {
        // Under a throwaway HOME: the run never checks the user's skills.
        let mut child = Command::new(env!("CARGO_BIN_EXE_graphicle-graph"))
            .args(&args)
            .env("HOME", dir.path())
            .env("USERPROFILE", dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // A watch that started would run until it is stopped.
        let started = Instant::now();
        while child.try_wait().unwrap().is_none() {
            if started.elapsed() > Duration::from_secs(60) {
                child.kill().unwrap();
                panic!("{args:?} started watching instead of refusing");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let output = child.wait_with_output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{args:?}: {stderr}");
        let expected = format!("error: `{option}` is not supported by Graphicle");
        assert!(stderr.contains(&expected), "{args:?}: {stderr}");
        assert!(
            !project.join(".graphicle").exists(),
            "{args:?} built a graph"
        );
    }
}
