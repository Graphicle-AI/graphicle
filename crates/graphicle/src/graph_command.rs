//! `graphicle graph <args>`: the graph engine's command line.
//!
//! The graph engine is its own program, `graphicle-graph`, installed beside
//! `graphicle`. `graphicle graph` finds it next to the running program, else
//! on `PATH`, and runs it with the words after `graph` unchanged. On Unix the
//! process becomes the engine, so standard input and output, signals and the
//! exit status are the engine's own. `graphicle graph` alone prints the
//! engine's help; the engine's JSON Lines mode is `graphicle graph --jsonl`.
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The graph engine's program name.
pub const ENGINE: &str = "graphicle-graph";

/// The exit status when the engine is not installed, as a shell reports a
/// command it cannot find.
pub const NOT_FOUND: i32 = 127;

/// The exit status when the engine was found but could not be started.
pub const NOT_STARTED: i32 = 126;

/// The engine's arguments when the command line `args` (the program name
/// first) is `graphicle graph …`: the words after `graph`. The global
/// options `--data-dir` and `--config` may come before `graph`; the engine
/// does not read them. `None` for any other command line.
pub fn requested(args: &[OsString]) -> Option<&[OsString]> {
    let mut i = 1;
    while let Some(word) = args.get(i) {
        let word = word.to_str()?;
        match word {
            "graph" => return Some(&args[i + 1..]),
            "--data-dir" | "--config" => i += 2,
            _ if word.starts_with("--data-dir=") || word.starts_with("--config=") => i += 1,
            _ => return None,
        }
    }
    None
}

/// Before the engine runs, and only when
/// standard error is a terminal, one line names the Graphicle project the
/// working directory belongs to; for `serve` it points to `graphicle mcp`,
/// which serves that project's code graph with its memory and documents.
/// The engine's own input, output and exit status are unchanged, nothing is
/// printed for help and version requests, outside a project or when piped,
/// and any error reading the store is ignored. `args` is the whole command
/// line, `engine_args` the words after `graph`.
pub fn note_project(args: &[OsString], engine_args: &[OsString]) {
    use std::io::IsTerminal;
    if !std::io::stderr().is_terminal() {
        return;
    }
    let Some(command) = noted_command(engine_args) else {
        return;
    };
    let directory = data_dir_of(args).unwrap_or_else(crate::local::data_dir);
    let Ok(cwd) = std::env::current_dir() else {
        return;
    };
    if let Some((id, name)) = project_of_folder(&directory, &cwd) {
        eprintln!("{}", project_note(&name, &id, command == "serve"));
    }
}

/// The engine command a note is printed for: the first word, unless the
/// words ask for help or the version.
pub fn noted_command(engine_args: &[OsString]) -> Option<&str> {
    let first = engine_args.first()?.to_str()?;
    let help = |word: &OsString| {
        matches!(
            word.to_str(),
            Some("--help" | "-h" | "--version" | "-V" | "help")
        )
    };
    (!engine_args.iter().any(help)).then_some(first)
}

/// `--data-dir` when it comes before `graph` on the command line `args`.
pub fn data_dir_of(args: &[OsString]) -> Option<PathBuf> {
    let mut i = 1;
    while let Some(word) = args.get(i) {
        let word = word.to_str()?;
        match word {
            "graph" => return None,
            "--data-dir" => return args.get(i + 1).map(PathBuf::from),
            "--config" => i += 2,
            _ => {
                if let Some(value) = word.strip_prefix("--data-dir=") {
                    return Some(PathBuf::from(value));
                }
                i += 1;
            }
        }
    }
    None
}

/// The store at `directory`, opened to read under the saved local session
/// without creating, migrating or writing anything, and that session's
/// principal.
fn read_only(directory: &Path) -> Option<(rusqlite::Connection, crate::store::Principal)> {
    let database = directory.join("reviews.sqlite3");
    let session = directory.join("local-session.json");
    if !database.is_file() || database.is_symlink() || session.is_symlink() {
        return None;
    }
    let bytes = std::fs::read(&session).ok().filter(|b| b.len() <= 8192)?;
    let session: crate::local::Session = serde_json::from_slice(&bytes).ok()?;
    let db = rusqlite::Connection::open_with_flags(
        &database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    db.busy_timeout(std::time::Duration::from_millis(200))
        .ok()?;
    db.pragma_update(None, "query_only", true).ok()?;
    let principal = crate::store::authorize_hash(
        &db,
        &session.workspace,
        &graphicle_memory::text::sha256(session.capability.as_bytes()),
        "read",
    )
    .ok()?;
    Some((db, principal))
}

/// The name of project `id`.
fn project_name(db: &rusqlite::Connection, id: &str) -> Option<String> {
    db.query_row("SELECT name FROM repo_projects WHERE id=?", [id], |row| {
        row.get(0)
    })
    .ok()
}

/// `(id, name)` of the project of `folder` (its nearest registered folder)
/// in the store at `directory`, read under the saved local session without
/// creating, migrating or writing anything.
pub fn project_of_folder(directory: &Path, folder: &Path) -> Option<(String, String)> {
    let (db, principal) = read_only(directory)?;
    let found = crate::folders::project_for_path(&db, &principal, folder).ok()??;
    let name = project_name(&db, &found.id)?;
    Some((found.id, name))
}

/// A folder whose project has a code graph, for the guard hooks of `connect
/// --graph`.
#[derive(Debug)]
pub struct GraphFolder {
    /// The project's name.
    pub name: String,
    /// The project's registered folder (canonical).
    pub root: PathBuf,
    /// Whether the code graph has nodes in the file asked about; `true` when
    /// none was.
    pub covers: bool,
}

/// The project of `folder` (read as [`project_of_folder`] reads it) when its
/// latest recorded code graph holds at least one code symbol
/// ([`crate::code_graph_arm::code_symbol`]): a
/// conversation or notes folder, whose graph holds no code, is not one. With
/// `file`, also whether that graph has nodes in it. The live-symbol table
/// (`gf_symbol_heads`, code symbols only, which a projection of the head set
/// keeps) answers the first question with one indexed row, so a hook stays
/// quick on a large project without code; a store whose table was never
/// built reads the graph's open nodes (`valid_to_episode IS NULL`) instead.
/// The second reads the open nodes through the index on each node's file
/// (`owner`).
pub fn project_with_graph(
    directory: &Path,
    folder: &Path,
    file: Option<&Path>,
) -> Option<GraphFolder> {
    let (db, principal) = read_only(directory)?;
    let found = crate::folders::project_for_path(&db, &principal, folder).ok()??;
    let built = db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM gf_symbol_state WHERE repository_id=?)",
            [&found.id],
            |row| row.get::<_, bool>(0),
        )
        .unwrap_or(false);
    let symbol = if built {
        db.query_row(
            "SELECT EXISTS(SELECT 1 FROM gf_symbol_heads WHERE repository_id=?)",
            [&found.id],
            |row| row.get::<_, bool>(0),
        )
        .ok()?
    } else {
        let mut select = db
            .prepare(
                "SELECT attributes FROM gf_nodes WHERE repository_id=? AND owner<>'' AND valid_to_episode IS NULL",
            )
            .ok()?;
        let mut rows = select.query([&found.id]).ok()?;
        let mut symbol = false;
        while let Some(row) = rows.next().ok()? {
            let text: String = row.get(0).ok()?;
            if serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&text)
                .is_ok_and(|attributes| crate::code_graph_arm::code_symbol(&attributes))
            {
                symbol = true;
                break;
            }
        }
        symbol
    };
    if !symbol {
        return None;
    }
    let covers = match file {
        None => true,
        Some(file) => {
            let file = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
            file.strip_prefix(&found.folder).ok().is_some_and(|relative| {
                let relative = relative.to_string_lossy().replace('\\', "/");
                db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM gf_nodes WHERE repository_id=? AND owner=? AND valid_to_episode IS NULL)",
                    [&found.id, &relative],
                    |row| row.get::<_, bool>(0),
                )
                .unwrap_or(false)
            })
        }
    };
    Some(GraphFolder {
        name: project_name(&db, &found.id)?,
        root: found.folder,
        covers,
    })
}

/// The line [`note_project`] prints.
pub fn project_note(name: &str, id: &str, serve: bool) -> String {
    if serve {
        format!(
            "graphicle: this folder is the Graphicle project \"{name}\" ({id}); `graphicle mcp` serves its code graph together with its memory and documents."
        )
    } else {
        format!(
            "graphicle: this folder is also the Graphicle project \"{name}\" ({id}); `graphicle context` searches its code graph together with its memory and documents."
        )
    }
}

/// The engine `graphicle graph` runs: `graphicle-graph` in the folder of
/// `program` (the running program as it was started, then with symbolic
/// links resolved), else in the first absolute folder of `path` (the `PATH`
/// variable) that has one. Relative `PATH` entries are skipped.
pub fn locate(program: Option<&Path>, path: Option<&OsStr>) -> Option<PathBuf> {
    let name = format!("{ENGINE}{}", std::env::consts::EXE_SUFFIX);
    let mut folders: Vec<PathBuf> = vec![];
    if let Some(program) = program {
        folders.extend(program.parent().map(Path::to_path_buf));
        if let Ok(resolved) = program.canonicalize() {
            folders.extend(resolved.parent().map(Path::to_path_buf));
        }
    }
    folders.extend(
        path.into_iter()
            .flat_map(std::env::split_paths)
            .filter(|folder| folder.is_absolute()),
    );
    folders
        .into_iter()
        .map(|folder| folder.join(&name))
        .find(|candidate| runnable(candidate))
}

fn runnable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// What `graphicle graph` prints when the engine is missing: where it looked
/// and how to install it.
pub fn missing(program: Option<&Path>) -> String {
    let beside = program
        .and_then(Path::parent)
        .map(|folder| format!(" next to this program ({}) or", folder.display()))
        .unwrap_or_default();
    format!(
        "graphicle: the graph engine `{ENGINE}` was not found{beside} on PATH. \
         Graphicle installs it beside `graphicle`: install with Homebrew \
         (brew install graphicle-ai/tap/graphicle) or from the release archive for \
         your platform (https://github.com/graphicle-ai/graphicle/releases), or put \
         `{ENGINE}` in the folder of `graphicle` or on PATH."
    )
}

/// The engine's command line for `graphicle graph args`.
fn command(args: &[OsString]) -> Result<Command, i32> {
    let program = std::env::current_exe().ok();
    let Some(engine) = locate(program.as_deref(), std::env::var_os("PATH").as_deref()) else {
        eprintln!("{}", missing(program.as_deref()));
        return Err(NOT_FOUND);
    };
    let mut command = Command::new(engine);
    if args.is_empty() {
        command.arg("--help");
    } else {
        command.args(args);
    }
    Ok(command)
}

/// Run `graphicle graph args` as the program's whole command: on Unix the
/// process becomes the engine, and this returns only when the engine could
/// not be found or started, with the exit status for `main`.
pub fn exec(args: &[OsString]) -> i32 {
    let mut command = match command(args) {
        Ok(command) => command,
        Err(code) => return code,
    };
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = command.exec();
        eprintln!(
            "graphicle: could not run {}: {error}",
            command.get_program().to_string_lossy()
        );
        NOT_STARTED
    }
    #[cfg(not(unix))]
    {
        started(&mut command)
    }
}

/// Run `graphicle graph args` as a child process with this process's
/// standard input and output, and return its exit status.
pub fn status(args: &[OsString]) -> i32 {
    match command(args) {
        Ok(mut command) => started(&mut command),
        Err(code) => code,
    }
}

fn started(command: &mut Command) -> i32 {
    match command.status() {
        // A signal ended the engine: report it as a failure.
        Ok(status) => status.code().unwrap_or(1),
        Err(error) => {
            eprintln!(
                "graphicle: could not run {}: {error}",
                command.get_program().to_string_lossy()
            );
            NOT_STARTED
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &str) -> Vec<OsString> {
        line.split_whitespace().map(OsString::from).collect()
    }

    #[test]
    fn graph_is_recognised_first_or_after_the_global_options_only() {
        let rest = |line: &str| {
            let args = words(line);
            requested(&args).map(|rest| rest.to_vec())
        };
        assert_eq!(rest("graphicle graph"), Some(vec![]));
        assert_eq!(
            rest("graphicle graph update . --force"),
            Some(words("update . --force"))
        );
        assert_eq!(rest("graphicle graph --help"), Some(words("--help")));
        assert_eq!(
            rest("graphicle --data-dir /tmp/d --config c.json graph query x"),
            Some(words("query x"))
        );
        assert_eq!(
            rest("graphicle --config=c.json graph graph"),
            Some(words("graph"))
        );
        for line in [
            "graphicle",
            "graphicle sync graph",
            "graphicle --version graph",
            "graphicle --data-dir",
            "graphicle graphs",
        ] {
            assert_eq!(rest(line), None, "{line}");
        }
    }

    #[cfg(unix)]
    fn install(folder: &Path, mode: u32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(folder).unwrap();
        let engine = folder.join(ENGINE);
        std::fs::write(&engine, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&engine, std::fs::Permissions::from_mode(mode)).unwrap();
        engine
    }

    #[cfg(unix)]
    #[test]
    fn the_engine_next_to_the_program_comes_before_path() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        let program = bin.join("graphicle");
        let on_path = dir.path().join("path");
        let first = install(&on_path.join("a"), 0o755);
        let second = install(&on_path.join("b"), 0o755);
        let path = std::env::join_paths([on_path.join("a"), on_path.join("b")]).unwrap();
        // Only on PATH: the first folder that has it.
        assert_eq!(locate(Some(&program), Some(&path)), Some(first.clone()));
        let path_ba = std::env::join_paths([on_path.join("b"), on_path.join("a")]).unwrap();
        assert_eq!(locate(Some(&program), Some(&path_ba)), Some(second));
        // Next to the program: it wins.
        let beside = install(&bin, 0o755);
        assert_eq!(locate(Some(&program), Some(&path)), Some(beside));
        // Without a program path, PATH alone.
        assert_eq!(locate(None, Some(&path)), Some(first));
    }

    #[cfg(unix)]
    #[test]
    fn a_link_to_the_program_finds_the_engine_beside_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let cellar = dir.path().join("cellar/bin");
        let engine = install(&cellar, 0o755);
        std::fs::write(cellar.join("graphicle"), "").unwrap();
        let links = dir.path().join("links");
        std::fs::create_dir_all(&links).unwrap();
        std::os::unix::fs::symlink(cellar.join("graphicle"), links.join("graphicle")).unwrap();
        let found = locate(Some(&links.join("graphicle")), None).unwrap();
        assert_eq!(
            found.canonicalize().unwrap(),
            engine.canonicalize().unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn missing_folders_files_without_execute_permission_and_relative_entries_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("bin/graphicle");
        assert_eq!(locate(Some(&program), None), None);
        install(&dir.path().join("bin"), 0o644);
        assert_eq!(locate(Some(&program), None), None);
        let relative = std::env::join_paths([Path::new("bin"), Path::new("")]).unwrap();
        assert_eq!(locate(None, Some(&relative)), None);
        let folder = dir.path().join("nested");
        std::fs::create_dir_all(folder.join(ENGINE)).unwrap();
        let path = std::env::join_paths([&folder]).unwrap();
        assert_eq!(locate(None, Some(&path)), None, "a folder is not a program");
    }

    /// The project note's command, data folder and text.
    #[test]
    fn the_project_note_names_the_command_store_and_project() {
        assert_eq!(noted_command(&words("serve --mcp")), Some("serve"));
        assert_eq!(noted_command(&words("query how")), Some("query"));
        for line in [
            "",
            "--help",
            "-h",
            "query --help",
            "--version",
            "-V",
            "help",
        ] {
            assert_eq!(noted_command(&words(line)), None, "{line}");
        }
        assert_eq!(data_dir_of(&words("graphicle graph serve")), None);
        assert_eq!(
            data_dir_of(&words("graphicle --data-dir /d graph serve")),
            Some(PathBuf::from("/d"))
        );
        assert_eq!(
            data_dir_of(&words("graphicle --config c --data-dir=/e graph x")),
            Some(PathBuf::from("/e"))
        );
        assert_eq!(
            data_dir_of(&words("graphicle graph --data-dir /f")),
            None,
            "after graph the words are the engine's"
        );
        assert!(project_note("app", "p1", true).contains("`graphicle mcp` serves"));
        assert!(project_note("app", "p1", false).contains("project \"app\" (p1)"));
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(project_of_folder(empty.path(), empty.path()), None);
    }

    #[test]
    fn the_missing_engine_message_says_where_it_looked_and_how_to_install() {
        let message = missing(Some(Path::new("/opt/graphicle/bin/graphicle")));
        for part in [
            "graphicle: the graph engine `graphicle-graph` was not found next to this program (/opt/graphicle/bin) or on PATH.",
            "brew install graphicle-ai/tap/graphicle",
            "release archive",
            "https://github.com/graphicle-ai/graphicle/releases",
        ] {
            assert!(message.contains(part), "{part}: {message}");
        }
        assert!(missing(None).contains("`graphicle-graph` was not found on PATH."));
    }
}
