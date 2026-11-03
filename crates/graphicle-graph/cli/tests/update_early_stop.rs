//! `update` stops early when nothing it reads changed since a full update
//! that found nothing to change: it leaves every output byte as the full
//! update leaves it. An update that writes a new graph is not enough (the
//! next one may still change it), and any change (a file's content, an
//! added, deleted or renamed file, an option or setting that changes the
//! output, another build, a graph.json written by another command, a
//! configuration file above the project) takes the full update again.
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
};

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
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let (root, out, home) = (base.join("project"), base.join("out"), base.join("home"));
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
    /// environment.
    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> String {
        let run = Command::new(env!("CARGO_BIN_EXE_graphicle-graph"))
            .args(args)
            .current_dir(&self.home)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("GRAPHICLE_OUT", &self.out)
            .envs(env.iter().copied())
            .output()
            .unwrap();
        assert!(run.status.success(), "{args:?}: {run:?}");
        String::from_utf8_lossy(&run.stdout).into_owned()
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

    // A new modification time without new content: the early stop, with
    // the manifest restamped as the full rebuild restamps it.
    let manifest = std::fs::read(project.out.join("manifest.json")).unwrap();
    touch(&root("pkg/main.py"), 3600);
    let touched = project.update();
    assert!(touched.contains(EARLY), "{touched}");
    assert_ne!(
        std::fs::read(project.out.join("manifest.json")).unwrap(),
        manifest
    );
    assert!(project.update().contains(EARLY));

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
