//! `graphicle graph` runs the graph engine, `graphicle-graph`, found next to
//! the program or else on PATH, with the words after `graph`, standard input,
//! output and exit status unchanged (src/graph_command.rs). A stand-in engine
//! checks the hand-over on every run. The built engine is checked too when
//! one is available: `GRAPHICLE_GRAPH_BIN`, or the engine crate's own
//! `target/{debug,release}/graphicle-graph`; otherwise that test says it was
//! skipped and passes. On Windows the stand-in is a small compiled program.
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// A fresh folder in Cargo's per-target scratch folder: on the file system of
/// the built program, so it can be hard-linked.
fn scratch() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("graph-command")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap()
}

/// This test's `graphicle` program in `folder` (a hard link, or a copy), so
/// that the folder it runs from holds only what the test puts there.
fn program_in(folder: &Path) -> PathBuf {
    std::fs::create_dir_all(folder).unwrap();
    let program = folder.join(crate::child_env::program("graphicle"));
    let built = Path::new(env!("CARGO_BIN_EXE_graphicle"));
    if std::fs::hard_link(built, &program).is_err() {
        std::fs::copy(built, &program).unwrap();
    }
    program
}

/// A stand-in engine in `folder`: prints its argument count, each argument
/// on a line and its standard input (shell built-ins only, so it needs no
/// PATH), writes `stderr` to standard error and exits with 7.
#[cfg(unix)]
fn stand_in(folder: &Path, stderr: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(folder).unwrap();
    let engine = folder.join("graphicle-graph");
    std::fs::write(
        &engine,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$#\" \"$@\"\n\
             while IFS= read -r line; do printf '%s\\n' \"$line\"; done\n\
             printf '%s\\n' '{stderr}' >&2\nexit 7\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&engine, std::fs::Permissions::from_mode(0o755)).unwrap();
    engine
}

/// The stand-in engine on Windows: [`compiled_stand_in`] in `folder`, with
/// the `stderr` text it writes in the file beside it.
#[cfg(windows)]
fn stand_in(folder: &Path, stderr: &str) -> PathBuf {
    std::fs::create_dir_all(folder).unwrap();
    let engine = folder.join(crate::child_env::program("graphicle-graph"));
    std::fs::copy(compiled_stand_in(), &engine).unwrap();
    std::fs::write(folder.join("stand-in-stderr.txt"), stderr).unwrap();
    engine
}

/// The Windows stand-in's program, compiled once per test run with the
/// toolchain's rustc (beside Cargo, else on PATH): the shell stand-in's
/// output, with its standard error text read from `stand-in-stderr.txt`.
#[cfg(windows)]
fn compiled_stand_in() -> &'static Path {
    const SOURCE: &str = r#"use std::io::{BufRead, Write};
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut out = std::io::stdout().lock();
    writeln!(out, "{}", args.len()).unwrap();
    for arg in &args {
        writeln!(out, "{arg}").unwrap();
    }
    for line in std::io::stdin().lock().lines() {
        writeln!(out, "{}", line.unwrap()).unwrap();
    }
    out.flush().unwrap();
    let text = std::env::current_exe().unwrap().with_file_name("stand-in-stderr.txt");
    eprintln!("{}", std::fs::read_to_string(text).unwrap());
    std::process::exit(7);
}
"#;
    static BUILT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BUILT.get_or_init(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("graph-command-stand-in-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("stand_in.rs");
        std::fs::write(&source, SOURCE).unwrap();
        let beside_cargo = Path::new(env!("CARGO")).with_file_name("rustc.exe");
        let rustc = if beside_cargo.is_file() {
            beside_cargo
        } else {
            PathBuf::from("rustc")
        };
        let built = dir.join("graphicle-graph.exe");
        let status = Command::new(rustc)
            .arg("--edition=2021")
            .arg("-o")
            .arg(&built)
            .arg(&source)
            .status()
            .unwrap();
        assert!(status.success(), "compiling the stand-in engine: {status}");
        built
    })
}

/// Run `program args` with only `path` on PATH and `home` as HOME, feeding
/// `input` (none: standard input is empty).
fn run(program: &Path, args: &[&str], path: &[&Path], home: &Path, input: &str) -> Output {
    let mut command = Command::new(program);
    crate::child_env::home(&mut command, home)
        .args(args)
        .env("PATH", std::env::join_paths(path).unwrap())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = crate::fork_guard::spawn(&mut command).unwrap();
    let mut stdin = child.stdin.take().unwrap();
    // A program that does not read its input may already have exited.
    let _ = stdin.write_all(input.as_bytes());
    drop(stdin);
    child.wait_with_output().unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn the_engine_beside_the_program_gets_the_words_input_and_status_unchanged() {
    let dir = scratch();
    let bin = dir.path().join("bin");
    let program = program_in(&bin);
    stand_in(&bin, "beside");
    // An engine on PATH is not used while one is beside the program.
    let elsewhere = dir.path().join("elsewhere");
    stand_in(&elsewhere, "on path");
    let output = run(
        &program,
        &["graph", "update", "two words", "--force", "-x", "--help"],
        &[&elsewhere],
        dir.path(),
        "first line\nsecond line\n",
    );
    assert_eq!(output.status.code(), Some(7), "{output:?}");
    assert_eq!(
        text(&output.stdout),
        "5\nupdate\ntwo words\n--force\n-x\n--help\nfirst line\nsecond line\n"
    );
    assert_eq!(text(&output.stderr), "beside\n");
    // Global options before `graph` stay with `graphicle`.
    let output = run(
        &program,
        &["--data-dir", "unused", "graph", "query", "q"],
        &[&elsewhere],
        dir.path(),
        "",
    );
    assert_eq!(output.status.code(), Some(7), "{output:?}");
    assert_eq!(text(&output.stdout), "2\nquery\nq\n");
}

#[test]
fn graph_alone_asks_the_engine_for_its_help() {
    let dir = scratch();
    let bin = dir.path().join("bin");
    let program = program_in(&bin);
    stand_in(&bin, "beside");
    let output = run(&program, &["graph"], &[], dir.path(), "");
    assert_eq!(output.status.code(), Some(7), "{output:?}");
    assert_eq!(text(&output.stdout), "1\n--help\n");
}

#[test]
fn an_engine_on_path_is_used_when_none_is_beside_the_program() {
    let dir = scratch();
    let program = program_in(&dir.path().join("bin"));
    let empty = dir.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let on_path = dir.path().join("on-path");
    stand_in(&on_path, "on path");
    let output = run(
        &program,
        &["graph", "explain", "X"],
        &[&empty, &on_path],
        dir.path(),
        "",
    );
    assert_eq!(output.status.code(), Some(7), "{output:?}");
    assert_eq!(text(&output.stdout), "2\nexplain\nX\n");
    assert_eq!(text(&output.stderr), "on path\n");
}

#[test]
fn a_missing_engine_is_reported_with_the_install_methods() {
    let dir = scratch();
    let bin = dir.path().join("bin");
    let program = program_in(&bin);
    let empty = dir.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let output = run(
        &program,
        &["graph", "update", "."],
        &[&empty],
        dir.path(),
        "",
    );
    assert_eq!(output.status.code(), Some(127), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = text(&output.stderr);
    for part in [
        "the graph engine `graphicle-graph` was not found next to this program",
        "brew install graphicle-ai/tap/graphicle",
        "release archive",
    ] {
        assert!(stderr.contains(part), "{part}: {stderr}");
    }
}

/// A built engine: `GRAPHICLE_GRAPH_BIN`, else the engine crate's own build.
fn built_engine() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("GRAPHICLE_GRAPH_BIN") {
        let path = PathBuf::from(path);
        assert!(path.is_file(), "GRAPHICLE_GRAPH_BIN: {}", path.display());
        return Some(path);
    }
    let engine = Path::new(env!("CARGO_MANIFEST_DIR")).join("../graphicle-graph/target");
    ["release", "debug"]
        .iter()
        .map(|profile| {
            engine
                .join(profile)
                .join(crate::child_env::program("graphicle-graph"))
        })
        .find(|path| path.is_file())
}

#[test]
fn the_built_engine_answers_through_graphicle_graph_as_it_does_directly() {
    let Some(engine) = built_engine() else {
        eprintln!(
            "skipped: no built graph engine (set GRAPHICLE_GRAPH_BIN, or build \
             graphicle-graph in the engine crate)"
        );
        return;
    };
    let dir = scratch();
    let bin = dir.path().join("bin");
    let program = program_in(&bin);
    #[cfg(unix)]
    std::os::unix::fs::symlink(&engine, bin.join("graphicle-graph")).unwrap();
    #[cfg(windows)]
    std::fs::copy(
        &engine,
        bin.join(crate::child_env::program("graphicle-graph")),
    )
    .unwrap();
    for args in [
        vec!["--version"],
        vec!["--help"],
        vec!["no-such-command"],
        vec!["query", "hello", "--graph", "missing/graph.json"],
    ] {
        let mut through = vec!["graph"];
        through.extend(&args);
        let graph = run(&program, &through, &[], dir.path(), "");
        let direct = run(&engine, &args, &[], dir.path(), "");
        assert_eq!(graph.status.code(), direct.status.code(), "{args:?}");
        assert_eq!(text(&graph.stdout), text(&direct.stdout), "{args:?}");
        assert_eq!(text(&graph.stderr), text(&direct.stderr), "{args:?}");
    }
    let version = run(&program, &["graph", "--version"], &[], dir.path(), "");
    assert_eq!(
        text(&version.stdout).trim(),
        format!("graphicle-graph {}", env!("CARGO_PKG_VERSION")),
        "the engine and the program share the release version"
    );
}
