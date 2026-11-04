//! `update` stops early when nothing it reads changed since a full update
//! that found nothing to change: it leaves every output byte as the full
//! update leaves it. An update that writes a new graph is not enough (the
//! next one may still change it), nor is one right after a file changed,
//! and any change (a file's content, size or times, an added, deleted or
//! renamed file, an option or setting that changes the output, another
//! build, a graph.json written by another command, a configuration file
//! above the project, a file extraction read outside the scan such as a
//! header in an ignored folder or a Sphinx `conf.py` above the project)
//! takes the full update again. A file that fails to
//! extract in a way a retry may pass (it cannot be read), or a JavaScript
//! workspace above the project, keeps it from ever stopping early; a file
//! its content fails (a parse error, a size limit) does not.
//!
//! The updates here trust a file's stats at once
//! (`GRAPHICLE_MTIME_GRANULARITY_MS=0`): by default a file changed in the
//! two seconds before an update keeps that update from recording.
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
};

#[allow(dead_code)]
mod common;

const EARLY: &str = "No relevant changes; outputs left untouched.";
const UNCHANGED: &str = "No code-graph topology changes detected; outputs left untouched.";
const RECORD: &str = ".graphicle_build_state.json";

struct Project {
    _temp: tempfile::TempDir,
    base: PathBuf,
    root: PathBuf,
    out: PathBuf,
    home: PathBuf,
}

impl Project {
    fn new() -> Project {
        Project::at("project")
    }

    /// The project in the folder `rel` of a new temporary folder.
    fn at(rel: &str) -> Project {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let (root, out, home) = (base.join(rel), base.join("out"), base.join("home"));
        std::fs::create_dir_all(root.join("pkg")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let write = |rel: &str, text: &str| std::fs::write(root.join(rel), text).unwrap();
        write(
            "pkg/parser.py",
            "def parse_all(paths):\n    return [parse(p) for p in paths]\n\n\ndef parse(path):\n    return Report(path)\n\n\nclass Report:\n    def __init__(self, path):\n        self.path = path\n\n    def render(self):\n        return str(self.path)\n",
        );
        write(
            "pkg/main.py",
            "from pkg.parser import parse_all\n\n\ndef main():\n    return [r.render() for r in parse_all(['a'])]\n",
        );
        write("pkg/util.py", "def helper():\n    return 2\n");
        write(
            "README.md",
            "# Parser\n\nCall `parse_all()` and then `Report.render()`.\n",
        );
        Project {
            _temp: temp,
            base,
            root,
            out,
            home,
        }
    }

    /// The engine's standard output for `args`, with `env` added to a clean
    /// environment, then its standard error (which says why an update did
    /// not stop early).
    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> String {
        let run = Command::new(env!("CARGO_BIN_EXE_graphicle-graph"))
            .args(args)
            .current_dir(&self.home)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("GRAPHICLE_OUT", &self.out)
            .env("GRAPHICLE_WATCH_TIMING", "1")
            .env("GRAPHICLE_MTIME_GRANULARITY_MS", "0")
            .envs(env.iter().copied())
            .output()
            .unwrap();
        assert!(run.status.success(), "{args:?}: {run:?}");
        format!(
            "{}\n--- stderr ---\n{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        )
    }

    fn update_with(&self, options: &[&str], env: &[(&str, &str)]) -> String {
        let root = self.root.to_string_lossy().into_owned();
        let mut args = vec!["update", root.as_str()];
        args.extend(options);
        self.run(&args, env)
    }

    fn update(&self) -> String {
        self.update_with(&[], &[])
    }

    /// Every output file but the cache, the record and the rebuild's lock
    /// and queue files.
    fn outputs(&self) -> BTreeMap<String, Vec<u8>> {
        let mut files = BTreeMap::new();
        let mut stack = vec![self.out.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                let name = path
                    .strip_prefix(&self.out)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                if path.is_dir() {
                    if name != "cache" {
                        stack.push(path);
                    }
                } else if name != RECORD
                    && !name.starts_with(".pending")
                    && !name.starts_with(".rebuild")
                {
                    files.insert(name, std::fs::read(&path).unwrap());
                }
            }
        }
        files
    }

    /// An update that takes the full rebuild; when it writes a new graph,
    /// another full one that finds nothing to change; then one that stops
    /// early.
    fn full_then_early(&self, case: &str) {
        self.full_then_early_with(case, &[], &[]);
    }

    fn full_then_early_with(&self, case: &str, options: &[&str], env: &[(&str, &str)]) {
        let full = self.update_with(options, env);
        assert!(
            !full.contains(EARLY),
            "{case}: the full rebuild should run:\n{full}"
        );
        if !full.contains(UNCHANGED) && !self.out.join(RECORD).exists() {
            let again = self.update_with(options, env);
            assert!(
                again.contains(UNCHANGED) && !again.contains(EARLY),
                "{case}: a new graph takes one more full rebuild:\n{again}"
            );
        }
        let early = self.update_with(options, env);
        assert!(early.contains(EARLY), "{case}: then stop early:\n{early}");
    }
}

fn touch(path: &Path, seconds_ago: u64) {
    let file = std::fs::File::options().append(true).open(path).unwrap();
    let when = std::time::SystemTime::now() - std::time::Duration::from_secs(seconds_ago);
    file.set_modified(when).unwrap();
}

#[test]
fn an_update_with_nothing_changed_writes_what_the_full_rebuild_writes() {
    let project = Project::new();
    let root = project.root.to_string_lossy().into_owned();
    // The measurement's commands: a code-only extract, a first update and an
    // update with nothing changed. The first update writes a new graph, so
    // the second takes the full rebuild too; it finds nothing to change.
    project.run(&["extract", &root, "--code-only", "--force"], &[]);
    let first = project.update();
    assert!(
        !first.contains(EARLY) && !first.contains(UNCHANGED),
        "{first}"
    );
    let second = project.update();
    assert!(
        second.contains(UNCHANGED) && !second.contains(EARLY),
        "{second}"
    );
    let built = project.outputs();
    assert!(built.contains_key("graph.json") && built.contains_key("manifest.json"));
    let graph_time = std::fs::metadata(project.out.join("graph.json"))
        .unwrap()
        .modified()
        .unwrap();
    let early = project.update();
    assert!(early.contains(EARLY), "{early}");
    assert!(
        project.outputs() == built,
        "the early stop changed an output"
    );
    let after = std::fs::metadata(project.out.join("graph.json"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(after, graph_time, "the early stop rewrote graph.json");
    // Without the record the same update takes the full rebuild, which finds
    // nothing to change and leaves the same bytes.
    std::fs::remove_file(project.out.join(RECORD)).unwrap();
    let full = project.update();
    assert!(full.contains(UNCHANGED) && !full.contains(EARLY), "{full}");
    assert!(
        project.outputs() == built,
        "the full rebuild changed an output"
    );
    // The worker count is not an output setting.
    let workers = project.update_with(&[], &[("GRAPHICLE_MAX_WORKERS", "1")]);
    assert!(workers.contains(EARLY), "{workers}");
}

#[test]
fn any_change_takes_the_full_rebuild() {
    let project = Project::new();
    let root = |rel: &str| project.root.join(rel);
    project.full_then_early("first update");

    // A content edit that adds a function: the update writes a new graph and
    // removes the record, the next one finds nothing to change and records,
    // the third stops.
    std::fs::write(
        root("pkg/util.py"),
        "def helper():\n    return 2\n\n\ndef twice():\n    return helper() * 2\n",
    )
    .unwrap();
    let wrote = project.update();
    assert!(
        !wrote.contains(EARLY) && !wrote.contains(UNCHANGED),
        "{wrote}"
    );
    assert!(
        !project.out.join(RECORD).exists(),
        "a new graph is not recorded"
    );
    let confirmed = project.update();
    assert!(confirmed.contains(UNCHANGED), "{confirmed}");
    assert!(project.out.join(RECORD).exists());
    assert!(project.update().contains(EARLY));
    std::fs::write(
        root("pkg/util.py"),
        "def helper():\n    return 3\n\n\ndef twice():\n    return helper() * 2\n",
    )
    .unwrap();
    project.full_then_early("content edit that keeps the graph");
    std::fs::write(root("pkg/extra.py"), "def extra():\n    return 4\n").unwrap();
    project.full_then_early("added file");
    std::fs::remove_file(root("pkg/extra.py")).unwrap();
    project.full_then_early("deleted file");
    std::fs::rename(root("pkg/util.py"), root("pkg/utils.py")).unwrap();
    project.full_then_early("renamed file");
    std::fs::write(root("NOTES.md"), "# Notes\n\nSee `helper()`.\n").unwrap();
    project.full_then_early("added document");

    // Options and settings that change what an update writes.
    project.full_then_early_with("--follow-symlinks", &["--follow-symlinks"], &[]);
    project.full_then_early("without --follow-symlinks");
    project.full_then_early_with("viz limit", &[], &[("GRAPHICLE_VIZ_NODE_LIMIT", "1")]);
    project.full_then_early("default viz limit");
    std::fs::write(root(".graphiclerc"), "language.inc=php\n").unwrap();
    project.full_then_early(".graphiclerc");
    let forced = project.update_with(&["--force"], &[]);
    assert!(!forced.contains(EARLY), "--force: {forced}");

    // Another build of the engine.
    let record_path = project.out.join(RECORD);
    let mut record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&record_path).unwrap()).unwrap();
    record["build"] = "another build".into();
    std::fs::write(&record_path, record.to_string()).unwrap();
    project.full_then_early("another build");

    // graph.json written by another command.
    let source = project.root.to_string_lossy().into_owned();
    project.run(&["extract", &source, "--code-only", "--force"], &[]);
    project.full_then_early("graph.json rewritten");

    // A Go module file above the project: extraction reads it for import
    // paths, the scan does not.
    let above = project.base.join("go.mod");
    std::fs::write(&above, "module example.com/one\n").unwrap();
    project.full_then_early("go.mod added above the root");
    std::fs::write(&above, "module example.com/two\n").unwrap();
    project.full_then_early("go.mod edited above the root");
}

/// A new modification time without new content takes the full update,
/// which restamps the manifest; the next update stops early and leaves the
/// manifest as that full update wrote it.
#[test]
fn a_new_modification_time_takes_the_full_update() {
    let project = Project::new();
    project.full_then_early("first update");
    let manifest = std::fs::read(project.out.join("manifest.json")).unwrap();
    touch(&project.root.join("pkg/main.py"), 3600);
    let touched = project.update();
    assert!(
        touched.contains(UNCHANGED)
            && !touched.contains(EARLY)
            && touched.contains("no early stop: file times"),
        "{touched}"
    );
    let restamped = project.outputs();
    assert_ne!(
        restamped["manifest.json"], manifest,
        "the full update restamps the file's time"
    );
    let early = project.update();
    assert!(early.contains(EARLY), "{early}");
    assert!(
        project.outputs() == restamped,
        "the early stop changed an output"
    );
}

/// Other content of the same size with the old modification time back
/// (`cp -p`, `rsync -a`, `tar -x`, `touch -r`): the status-change time moved,
/// so the update is a full one.
#[cfg(unix)]
#[test]
fn an_edit_that_keeps_size_and_time_takes_the_full_update() {
    let project = Project::new();
    project.full_then_early("first update");
    let path = project.root.join("pkg/util.py");
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    std::fs::write(&path, "def helper():\n    return 7\n").unwrap();
    let file = std::fs::File::options().append(true).open(&path).unwrap();
    file.set_modified(modified).unwrap();
    let after = project.update();
    assert!(
        !after.contains(EARLY) && after.contains("no early stop: file stats"),
        "{after}"
    );
}

/// A file changed just before an update (within the granularity) may change
/// again with the same size and times, so that update records nothing; once
/// the granularity has passed, the next full update records.
#[test]
fn an_update_right_after_a_change_is_not_recorded() {
    let project = Project::new();
    project.full_then_early("first update");
    let slow = [("GRAPHICLE_MTIME_GRANULARITY_MS", "3000")];
    touch(&project.root.join("pkg/main.py"), 0);
    let racy = project.update_with(&[], &slow);
    assert!(
        racy.contains(UNCHANGED)
            && racy.contains("not recorded: a file changed just before the scan"),
        "{racy}"
    );
    assert!(!project.out.join(RECORD).exists());
    std::thread::sleep(std::time::Duration::from_millis(3200));
    let settled = project.update_with(&[], &slow);
    assert!(
        settled.contains(UNCHANGED) && !settled.contains("not recorded"),
        "{settled}"
    );
    let early = project.update_with(&[], &slow);
    assert!(early.contains(EARLY), "{early}");
}

/// A file that fails to extract because of its content (a JSON file over the
/// extractor's 1 MiB limit, a YAML file the parser refuses) fails the same
/// way on every update: the update records it with the other files, stops
/// early on the second update with nothing changed, and takes the full
/// update again once the file changes.
#[test]
fn a_file_its_content_fails_stops_early_until_it_changes() {
    let project = Project::new();
    let big = project.root.join("big.json");
    let json = |fill: &str| format!("{{\"data\": \"{}\"}}\n", fill.repeat(1_100_000));
    std::fs::write(&big, json("x")).unwrap();
    std::fs::write(project.root.join("tabbed.yaml"), "a:\n\tb: 1\n").unwrap();
    project.full_then_early("files their content fails");
    // Neither file extracted: the manifest stamps neither.
    let manifest = std::fs::read_to_string(project.out.join("manifest.json")).unwrap();
    assert!(
        !manifest.contains("big.json") && !manifest.contains("tabbed.yaml"),
        "{manifest}"
    );
    std::fs::write(&big, json("y")).unwrap();
    let changed = project.update();
    assert!(
        !changed.contains(EARLY) && changed.contains("no early stop: file"),
        "{changed}"
    );
    assert!(!changed.contains("not recorded"), "{changed}");
    let early = project.update();
    assert!(early.contains(EARLY), "{early}");
}

/// A file that cannot be read fails in a way a retry may pass: while it
/// fails, no update is recorded; once it reads again, the full update takes
/// it in and the update after the next stops early. A process that reads
/// files without read permission (root) cannot fail that way: the test then
/// says so and checks nothing.
#[cfg(unix)]
#[test]
fn a_file_that_cannot_be_read_keeps_the_record_away() {
    use std::os::unix::fs::PermissionsExt;
    let project = Project::new();
    project.full_then_early("first update");
    let locked = project.root.join("pkg/locked.py");
    std::fs::write(&locked, "def locked_helper():\n    return 1\n").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&locked).is_ok() {
        eprintln!("skipped: this process reads files without read permission");
        return;
    }
    for run in 0..2 {
        let update = project.update();
        assert!(!update.contains(EARLY), "run {run}: {update}");
        assert!(!project.out.join(RECORD).exists(), "run {run}: recorded");
    }
    let last = project.update();
    assert!(
        last.contains("not recorded: a file failed to extract and may pass on a retry"),
        "{last}"
    );
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
    project.full_then_early("the file reads again");
    let graph = std::fs::read_to_string(project.out.join("graph.json")).unwrap();
    assert!(graph.contains("locked_helper"), "{graph}");
}

/// A JavaScript workspace root above the project: resolution reads its
/// member packages beside the project, which no record covers, so the
/// update never stops early.
#[test]
fn a_workspace_above_the_project_never_stops_early() {
    let project = Project::at("packages/app");
    std::fs::write(
        project.base.join("pnpm-workspace.yaml"),
        "packages:\n  - 'packages/*'\n",
    )
    .unwrap();
    let lib = project.base.join("packages/lib");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(
        lib.join("package.json"),
        r#"{"name":"lib","main":"index.js"}"#,
    )
    .unwrap();
    std::fs::write(
        lib.join("index.js"),
        "export function shared() { return 1; }\n",
    )
    .unwrap();
    std::fs::write(
        project.root.join("index.js"),
        "import { shared } from 'lib';\nexport function run() { return shared(); }\n",
    )
    .unwrap();
    for run in 0..3 {
        let update = project.update();
        assert!(
            !update.contains(EARLY)
                && update.contains("no early stop: a JavaScript workspace above the root"),
            "run {run}: {update}"
        );
        assert!(!project.out.join(RECORD).exists(), "run {run}: recorded");
    }
}

/// Configuration files extraction reads in the folders above the project:
/// a tsconfig.json, a package.json, a version control folder.
#[test]
fn configuration_above_the_project_takes_the_full_update() {
    let project = Project::new();
    project.full_then_early("first update");
    std::fs::write(
        project.base.join("tsconfig.json"),
        r#"{"compilerOptions":{"baseUrl":"."}}"#,
    )
    .unwrap();
    project.full_then_early("tsconfig.json added above the root");
    std::fs::write(project.base.join("package.json"), r#"{"name":"outer"}"#).unwrap();
    project.full_then_early("package.json added above the root");
    std::fs::create_dir_all(project.base.join(".git")).unwrap();
    project.full_then_early(".git added above the root");
}

/// A converted Office document: the update stops early while it is the
/// same, and a new version of it takes the full update.
#[test]
fn an_office_document_stops_early_until_it_changes() {
    let project = Project::new();
    let notes = project.root.join("notes.docx");
    let version = |text: &str| common::word_document(&[(Some("Heading1"), "Notes"), (None, text)]);
    std::fs::write(&notes, version("Call `helper()` first.")).unwrap();
    project.full_then_early("first update");
    std::fs::write(&notes, version("Call `parse_all()` first.")).unwrap();
    touch(&notes, 0);
    project.full_then_early("the document changed");
}

/// HOME is a setting (it bounds where JavaScript resolution looks for a
/// package.json); the hooks' change lists are not.
#[test]
fn home_counts_and_hook_change_lists_do_not() {
    let project = Project::new();
    project.full_then_early("first update");
    let hook = project.update_with(&[], &[("GRAPHICLE_CHANGED_JSON", "[\"pkg/util.py\"]")]);
    assert!(hook.contains(EARLY), "{hook}");
    let other_home = project.base.join("home2");
    std::fs::create_dir_all(&other_home).unwrap();
    let home = other_home.to_string_lossy().into_owned();
    let moved = project.update_with(&[], &[("HOME", home.as_str())]);
    assert!(
        !moved.contains(EARLY) && moved.contains("no early stop: settings"),
        "{moved}"
    );
}

/// The update after a change takes the full rebuild and writes what a full
/// rebuild writes: another update forced past the check leaves the same
/// outputs.
fn full_and_same(project: &Project, case: &str, env: &[(&str, &str)]) {
    let update = project.update_with(&[], env);
    assert!(!update.contains(EARLY), "{case}: no early stop:\n{update}");
    let written = project.outputs();
    let forced = project.update_with(&["--force"], env);
    assert!(!forced.contains(EARLY), "{case}: {forced}");
    assert!(
        project.outputs() == written,
        "{case}: the update wrote other outputs than a full rebuild"
    );
}

/// A quoted C include that resolves to a header the scan does not cover (in
/// an ignored folder, listed only as a folder): the record keeps the
/// header's stats, so removing it takes the full update, whose graph no
/// longer resolves the include to it.
#[test]
fn a_header_outside_the_scan_takes_the_full_update() {
    let project = Project::new();
    let root = &project.root;
    std::fs::create_dir_all(root.join("generated")).unwrap();
    std::fs::write(root.join(".gitignore"), "generated/\n").unwrap();
    std::fs::write(root.join("generated/config.h"), "#define LEVEL 1\n").unwrap();
    std::fs::write(
        root.join("main.c"),
        "#include \"generated/config.h\"\n\nint level(void) { return LEVEL; }\n",
    )
    .unwrap();
    project.full_then_early("a header in an ignored folder");
    let graph = || std::fs::read_to_string(project.out.join("graph.json")).unwrap();
    let before = graph();
    std::fs::remove_file(root.join("generated/config.h")).unwrap();
    full_and_same(&project, "the header removed", &[]);
    assert_ne!(graph(), before, "the include no longer resolves");
    // A header added where an include found none: the full update replays
    // the cached result that found none, and so does the early stop.
    let early = project.update();
    assert!(early.contains(EARLY), "without the header: {early}");
    std::fs::write(root.join("generated/config.h"), "#define LEVEL 2\n").unwrap();
    project.update();
    let written = project.outputs();
    project.update_with(&["--force"], &[]);
    assert!(project.outputs() == written, "the header back");
}

/// A Sphinx `conf.py` in a folder above the project decides whether its
/// `.txt` files are pages (`--docs`): adding one takes the full update,
/// whose graph then holds the page.
#[test]
fn a_sphinx_configuration_above_the_project_takes_the_full_update() {
    let project = Project::new();
    let docs = [("GRAPHICLE_DOCS", "1")];
    std::fs::create_dir_all(project.root.join("docs")).unwrap();
    std::fs::write(
        project.root.join("docs/intro.txt"),
        "Introduction\n============\n\nCall :func:`parse_all` first.\n",
    )
    .unwrap();
    project.full_then_early_with("no Sphinx project", &[], &docs);
    let graph = || std::fs::read_to_string(project.out.join("graph.json")).unwrap();
    assert!(!graph().contains("Introduction"), "{}", graph());
    std::fs::write(
        project.base.join("conf.py"),
        "source_suffix = ['.rst', '.txt']\n",
    )
    .unwrap();
    full_and_same(&project, "conf.py added above the root", &docs);
    assert!(graph().contains("Introduction"), "{}", graph());
}
