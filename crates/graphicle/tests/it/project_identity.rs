//! One project identity: the folder a command runs in decides
//! its project. Local syncs record their folders as aliases of the project
//! they synced into; the resolver finds the nearest registered folder
//! through aliases and `local:` keys; `--project` takes an id, a key or a
//! folder; the saved default applies only outside every registered folder.
//! A PR scratch project shares its folder project's `stable_id`s, nested
//! projects never export the same package twice, and the Python host's
//! workspace-source links are no longer read.
use graphicle_runtime::platform::CanonicalExt;
use graphicle_runtime::{
    agent_setup, capture, folders, graph_projection, jobs, local, memory,
    store::{Store, s},
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

struct Fixture {
    _dir: TempDir,
    store: Store,
    session: local::Session,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path().join("store")).unwrap();
        let session = local::session(&store, true).unwrap();
        let root = dir.path().canonical().unwrap().join("work");
        std::fs::create_dir_all(&root).unwrap();
        Self {
            _dir: dir,
            store,
            session,
            root,
        }
    }
    fn ws(&self) -> &str {
        &self.session.workspace
    }
    fn key(&self) -> &str {
        &self.session.capability
    }
    /// A folder under the fixture's root with one note naming it.
    fn folder(&self, relative: &str) -> PathBuf {
        let path = self.root.join(relative);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("notes.txt"), format!("Notes of {relative}.\n")).unwrap();
        path
    }
    fn write(&self, folder: &Path, files: &[(&str, &str)]) {
        for (path, text) in files {
            let target = folder.join(path);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(target, text).unwrap();
        }
    }
    fn sync(&mut self, folder: &Path, project: Option<&str>) -> String {
        let synced = local::sync(&self.store, &mut self.session, folder, project).unwrap();
        s(&synced, "repository_id").to_string()
    }
    /// The project `path` resolves to through its registered folders.
    fn at(&self, path: &Path) -> Option<String> {
        self.store
            .project_for_path(self.ws(), self.key(), path)
            .unwrap()
            .map(|found| found.id)
    }
    fn resolve(
        &self,
        requested: Option<&str>,
        cwd: &Path,
        fallback: Option<&str>,
    ) -> graphicle_runtime::Result<folders::Resolved> {
        self.store
            .resolve_project(self.ws(), self.key(), requested, Some(cwd), fallback)
    }
    /// Run every queued job as a worker would (projections and link runs).
    fn drain(&self) {
        while let Some(job) = self.store.lease_job().unwrap() {
            let result = match job.kind.as_str() {
                "project" => jobs::run_projection(&self.store, &job),
                "link" => jobs::run_link(&self.store, &job),
                _ => Ok(json!({"skipped": true})),
            };
            self.store.finish_job(&job, result).unwrap();
        }
    }
    /// Commit `files` into `repository` through the sync API, as a remote
    /// client (or `graphicle pr`'s tree sync) does.
    fn commit(&self, repository: &str, files: &[(&str, &str)], sha: &str) {
        let args = |mut value: Value| {
            value["repository_id"] = json!(repository);
            value
        };
        let status = self
            .store
            .project_status(self.ws(), self.key(), &args(json!({})))
            .unwrap();
        let manifest: Vec<Value> = files
            .iter()
            .map(|(path, text)| json!({"path":path,"sha256":graphicle_memory::text::sha256(text.as_bytes()),"bytes":text.len()}))
            .collect();
        let begin = self
            .store
            .sync_prepare(
                self.ws(),
                self.key(),
                &args(json!({"expected_snapshot":status["repository"]["snapshot"],"metadata":{"commit":sha},"manifest":manifest})),
            )
            .unwrap();
        let upload: Vec<Value> = files
            .iter()
            .map(|(path, text)| json!({"path":path,"content":text}))
            .collect();
        self.store
            .sync_upload(
                self.ws(),
                self.key(),
                &args(json!({"sync_id":begin["sync_id"],"files":upload})),
            )
            .unwrap();
        self.store
            .sync_commit(
                self.ws(),
                self.key(),
                &args(json!({"sync_id":begin["sync_id"]})),
            )
            .unwrap();
    }
    /// `node id → (stable_id, repo)` of the project's current episode.
    fn identities(&self, repository: &str) -> BTreeMap<String, (Value, Value)> {
        self.store
            .read(|db| {
                let episode = graph_projection::current_episode(db, repository)?;
                let graph = graph_projection::resident(db, repository, &episode)?;
                Ok(graph
                    .nodes()
                    .iter()
                    .map(|node| {
                        (
                            node.id.as_str().unwrap_or_default().to_string(),
                            (
                                node.attributes["stable_id"].clone(),
                                node.attributes["repo"].clone(),
                            ),
                        )
                    })
                    .collect())
            })
            .unwrap()
    }
    fn binary(&self, cwd: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_graphicle"));
        command
            .current_dir(cwd)
            .arg("--data-dir")
            .arg(self.store.directory.as_path());
        command
    }
}

/// Run `command` with `input` on standard input; stdout as text.
fn run(mut command: Command, input: &str) -> String {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = crate::fork_guard::spawn(&mut command).unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The resolver table: nested, symlinked and moved folders, a
/// `--project` sync into a project created in the web app, a project synced
/// before folders were recorded, and a deleted project.
#[test]
fn the_nearest_registered_folder_decides_through_aliases_and_keys() {
    let mut f = Fixture::new();
    let a = f.folder("a");
    let sub = f.folder("a/sub");
    let b = f.folder("b");
    let pa = f.sync(&a, None);
    let psub = f.sync(&sub, None);
    let pb = f.sync(&b, None);
    assert_ne!(pa, psub, "a subfolder synced alone is a project of its own");
    // Nested: the nearest registered folder wins; files and deeper folders
    // belong to it.
    std::fs::create_dir_all(a.join("x/y")).unwrap();
    std::fs::create_dir_all(sub.join("deep")).unwrap();
    assert_eq!(f.at(&a.join("x/y")), Some(pa.clone()));
    assert_eq!(f.at(&a.join("notes.txt")), Some(pa.clone()));
    assert_eq!(f.at(&sub.join("deep")), Some(psub.clone()));
    assert_eq!(f.at(&f.root), None);
    let found = f
        .store
        .project_for_path(f.ws(), f.key(), &a.join("x"))
        .unwrap()
        .unwrap();
    assert_eq!((found.folder.as_path(), found.via), (a.as_path(), "alias"));
    // Symlinked: the canonical folder decides.
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&a, f.root.join("link")).unwrap();
        assert_eq!(f.at(&f.root.join("link/x")), Some(pa.clone()));
    }
    // A project synced before folders were recorded is found by its key.
    f.store
        .connect()
        .unwrap()
        .execute("DELETE FROM repo_folders WHERE repository_id=?", [&pb])
        .unwrap();
    let found = f
        .store
        .project_for_path(f.ws(), f.key(), &b)
        .unwrap()
        .unwrap();
    assert_eq!((found.id.as_str(), found.via), (pb.as_str(), "key"));
    // A project created in the web app (a random key), synced with
    // --project: its folder is an alias, found by the resolver and by the
    // capture hooks, and a plain re-sync of the folder stays in it.
    let web = s(
        &f.store
            .register(
                f.ws(),
                f.key(),
                &json!({"project_key":"11111111-2222-4333-8444-555555555555","name":"Web notes"}),
            )
            .unwrap(),
        "id",
    )
    .to_string();
    let c = f.folder("c");
    assert_eq!(f.sync(&c, Some(&web)), web);
    assert_eq!(f.at(&c), Some(web.clone()));
    assert_eq!(
        capture::project_for_cwd(&f.store, &f.session, &c).unwrap(),
        Some(web.clone())
    );
    assert_eq!(f.sync(&c, None), web);
    // Moved: the new path is a new folder until it is synced into the
    // project again, which records it.
    let m = f.folder("m");
    let pm = f.sync(&m, None);
    let moved = f.root.join("m2");
    std::fs::rename(&m, &moved).unwrap();
    assert_eq!(f.at(&moved), None);
    assert_eq!(f.at(&m), None);
    assert_eq!(f.sync(&moved, Some(&pm)), pm);
    assert_eq!(f.at(&moved), Some(pm.clone()));
    // Deleted: its folders go with it; the nested project stays.
    let snapshot = f
        .store
        .project_status(f.ws(), f.key(), &json!({"repository_id": pa}))
        .unwrap()["repository"]["snapshot"]
        .clone();
    let deleted = f
        .store
        .maintain(
            f.ws(),
            f.key(),
            "project.delete",
            &json!({"repository_id": pa, "expected_snapshot": snapshot}),
        )
        .unwrap();
    assert_eq!(deleted["receipt"]["verified_local"], true, "{deleted:#}");
    assert_eq!(f.at(&a.join("x/y")), None);
    assert_eq!(f.at(&sub), Some(psub.clone()));
}

/// `--project` takes an id, a key, a recorded folder key or a folder; the
/// working directory comes next; the saved default only outside every
/// registered folder.
#[test]
fn a_project_is_named_by_id_key_or_folder_and_the_folder_comes_before_the_default() {
    let mut f = Fixture::new();
    let a = f.folder("a");
    let b = f.folder("b");
    let pa = f.sync(&a, None);
    let pb = f.sync(&b, None);
    assert_eq!(f.session.project.as_deref(), Some(pb.as_str()));
    std::fs::create_dir_all(a.join("x/y")).unwrap();
    let key = folders::folder_key(&a);
    for (value, cwd) in [
        (pa.as_str(), f.root.as_path()),
        (key.as_str(), f.root.as_path()),
        (a.to_str().unwrap(), f.root.as_path()),
        ("x/y", a.as_path()),
    ] {
        let resolved = f.resolve(Some(value), cwd, Some(&pb)).unwrap();
        assert_eq!(
            (resolved.id.as_str(), resolved.source),
            (pa.as_str(), "requested"),
            "{value}"
        );
        assert_eq!(resolved.name, "a");
    }
    assert!(f.resolve(Some("no-such-project"), &a, None).is_err());
    let outside = f.root.join("elsewhere");
    std::fs::create_dir_all(&outside).unwrap();
    assert!(
        f.resolve(Some(outside.to_str().unwrap()), &a, None)
            .unwrap_err()
            .message
            .contains("sync it first")
    );
    // The working directory beats the saved default...
    let resolved = f.resolve(None, &a.join("x/y"), Some(&pb)).unwrap();
    assert_eq!(
        (resolved.id.as_str(), resolved.source, resolved.folder),
        (pa.as_str(), "folder", Some(a.clone()))
    );
    // ...which applies only outside every registered folder.
    let resolved = f.resolve(None, &outside, Some(&pb)).unwrap();
    assert_eq!(
        (resolved.id.as_str(), resolved.source),
        (pb.as_str(), "saved_default")
    );
    assert!(f.resolve(None, &outside, None).is_err());
    // A PR scratch project is never a folder's project.
    let scratch = f.store.pr_scratch_for(f.ws(), f.key(), &pa).unwrap();
    assert_eq!(scratch["created"], true);
    assert_eq!(scratch["project"], pa.as_str());
    let again = f.store.pr_scratch_for(f.ws(), f.key(), &pa).unwrap();
    assert_eq!(
        (again["id"].clone(), again["created"].clone()),
        (scratch["id"].clone(), json!(false))
    );
    let other = f.folder("other");
    assert!(local::sync(&f.store, &mut f.session, &other, Some(s(&scratch, "id"))).is_err());
}

/// Inside a folder, `status`, `context`, `mcp`, `connect` and the
/// session-start hook all act on that folder's project, although the last
/// sync (and so the saved default) was another folder, and a folder
/// synced into a web project is found by the hooks and by `connect`.
#[test]
fn inside_a_folder_every_command_and_hook_acts_on_its_project() {
    let mut f = Fixture::new();
    let a = f.folder("a");
    let b = f.folder("b");
    let pa = f.sync(&a, None);
    let pb = f.sync(&b, None);
    assert_eq!(f.session.project.as_deref(), Some(pb.as_str()));
    let status: Value = serde_json::from_str(&run(
        {
            let mut command = f.binary(&a);
            command.arg("status");
            command
        },
        "",
    ))
    .unwrap();
    assert_eq!(status["repository"]["id"], pa.as_str(), "{status:#}");
    let context = run(
        {
            let mut command = f.binary(&a.join("."));
            command.args(["context", "notes"]);
            command
        },
        "",
    );
    assert!(context.contains("Notes of a."), "{context}");
    assert!(!context.contains("Notes of b."), "{context}");
    // Outside every folder the saved default still answers.
    let status: Value = serde_json::from_str(&run(
        {
            let mut command = f.binary(&f.root);
            command.arg("status");
            command
        },
        "",
    ))
    .unwrap();
    assert_eq!(status["repository"]["id"], pb.as_str());
    // The stdio server.
    let requests = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"i2","version":"0"}}}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"repository_status","arguments":{}}}),
    ]
    .map(|r| r.to_string())
    .join("\n")
        + "\n";
    let served = run(
        {
            let mut command = f.binary(&a);
            command.arg("mcp");
            command
        },
        &requests,
    );
    let answer: Value = served
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|line| line["id"] == 2)
        .unwrap();
    assert!(answer.to_string().contains(&pa), "{answer:#}");
    assert!(!answer.to_string().contains(&pb), "{answer:#}");
    // The session-start hook.
    f.store
        .work_state_write(
            f.ws(),
            f.key(),
            &json!({"repository_id":pa,"list":"a plan","fields":{"task":"finish folder a","status":"doing"}}),
        )
        .unwrap();
    let start = json!({"session_id":"5c0ffee0-1111-4222-8333-944445555666","cwd":a.join("x").to_string_lossy(),"hook_event_name":"SessionStart","source":"startup"});
    std::fs::create_dir_all(a.join("x")).unwrap();
    let hooked: Value = serde_json::from_str(&run(
        {
            let mut command = f.binary(&f.root);
            command.args(["hook", "claude-code", "session-start"]);
            command
        },
        &start.to_string(),
    ))
    .unwrap();
    assert!(
        s(&hooked["hookSpecificOutput"], "additionalContext").contains("finish folder a"),
        "{hooked:#}"
    );
    // connect: a portable entry in the folder for Claude Code; a named entry
    // for a user-level client; an explicit --project is named in the entry.
    let store = f.store.clone();
    let connect = |client: &str, path: Option<&Path>, project: Option<&str>, cwd: &Path| {
        agent_setup::connect(
            &store,
            &agent_setup::Connect {
                client,
                config_path: path,
                project,
                name: None,
                replace: false,
                hooks: false,
                work_state: false,
                graph: false,
                allow_learning: false,
                // The whole server, whose entries name no tool set.
                tools: Some(graphicle_runtime::api::ToolSet::All),
                guards: false,
                cwd,
            },
        )
        .unwrap()
    };
    let portable = connect("claude-code", None, None, &a);
    assert_eq!(portable["entry"], "graphicle");
    assert_eq!(portable["project"]["id"], pa.as_str());
    assert_eq!(portable["project"]["source"], "folder");
    let written: Value =
        serde_json::from_slice(&std::fs::read(a.join(".mcp.json")).unwrap()).unwrap();
    let args = written["mcpServers"]["graphicle"]["args"].clone();
    assert_eq!(
        args,
        json!([
            "--data-dir",
            f.store.directory.canonical().unwrap(),
            "mcp",
            "--project",
            "."
        ]),
        "{written:#}"
    );
    // That entry, started in the folder, serves the folder's project.
    let served = run(
        {
            let mut command = Command::new(env!("CARGO_BIN_EXE_graphicle"));
            command
                .current_dir(&a)
                .args(args.as_array().unwrap().iter().map(|a| a.as_str().unwrap()));
            command
        },
        &requests,
    );
    assert!(served.contains(&pa) && !served.contains(&pb), "{served}");
    let command = s(&written["mcpServers"]["graphicle"], "command");
    assert!(
        command == "graphicle" || Path::new(command).is_absolute(),
        "{command}"
    );
    let cursor = f.root.join("cursor.json");
    let named = connect("cursor", Some(&cursor), None, &a);
    assert_eq!(named["entry"], "graphicle-a");
    let written: Value = serde_json::from_slice(&std::fs::read(&cursor).unwrap()).unwrap();
    let args = written["mcpServers"]["graphicle-a"]["args"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(args[args.len() - 2..], [json!("--project"), json!(pa)]);
    // Another project's entry sits beside it, under its own name.
    let other = connect("cursor", Some(&cursor), Some(&pb), &a);
    assert_eq!(other["entry"], "graphicle-b");
    let written: Value = serde_json::from_slice(&std::fs::read(&cursor).unwrap()).unwrap();
    assert_eq!(written["mcpServers"].as_object().unwrap().len(), 2);
    // A Claude Code file elsewhere names the project.
    let elsewhere = f.root.join("shared.json");
    let named_file = connect("claude-code", Some(&elsewhere), None, &a);
    assert_eq!(named_file["project"]["source"], "folder");
    let written: Value = serde_json::from_slice(&std::fs::read(&elsewhere).unwrap()).unwrap();
    let args = written["mcpServers"]["graphicle"]["args"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(args[args.len() - 2..], [json!("--project"), json!(pa)]);
    let pinned = connect("vscode", None, Some(&pb), &a);
    assert_eq!(pinned["project"]["source"], "requested");
    let written: Value =
        serde_json::from_slice(&std::fs::read(a.join(".vscode/mcp.json")).unwrap()).unwrap();
    let args = written["servers"]["graphicle"]["args"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(args[args.len() - 2..], [json!("--project"), json!(pb)]);
    // A folder synced into a web project is found by connect too.
    let web = s(
        &f.store
            .register(
                f.ws(),
                f.key(),
                &json!({"project_key":"web-made","name":"Web notes"}),
            )
            .unwrap(),
        "id",
    )
    .to_string();
    let c = f.folder("c");
    f.sync(&c, Some(&web));
    let found = connect("claude-code", None, None, &c);
    assert_eq!(found["project"]["id"], web.as_str());
    assert_eq!(found["project"]["source"], "folder");
    assert_eq!(
        agent_setup::entry_name("My App (v2)!"),
        "graphicle-my-app-v2"
    );
    assert_eq!(agent_setup::entry_name("…"), "graphicle-project");
}

/// A PR scratch project is projected with its folder project's id as the
/// identity scope: the same code has the same `stable_id`s in
/// both, while its nodes name the scratch project, where their episode is.
#[test]
fn a_pr_scratch_project_shares_its_folder_projects_stable_ids() {
    let mut f = Fixture::new();
    let a = f.folder("a");
    let files = [
        (
            "pkg/core.py",
            "def alpha():\n    return 1\n\ndef beta():\n    return alpha()\n",
        ),
        (
            "pkg/util.py",
            "from pkg.core import beta\n\ndef gamma():\n    return beta()\n",
        ),
    ];
    f.write(&a, &files);
    let project = f.sync(&a, None);
    let scratch = s(
        &f.store.pr_scratch_for(f.ws(), f.key(), &project).unwrap(),
        "id",
    )
    .to_string();
    let mut tree: Vec<(&str, &str)> = files.to_vec();
    tree.push(("notes.txt", "Notes of a.\n"));
    f.commit(&scratch, &tree, "aaa1110000000000000000000000000000000000");
    f.drain();
    let ours = f.identities(&project);
    let theirs = f.identities(&scratch);
    assert!(ours.len() > 4, "{ours:?}");
    assert_eq!(
        ours.keys().collect::<Vec<_>>(),
        theirs.keys().collect::<Vec<_>>()
    );
    for (node, (stable_id, repo)) in &ours {
        let (scratch_id, scratch_repo) = &theirs[node];
        assert_eq!(stable_id, scratch_id, "{node}");
        assert_eq!(repo, project.as_str());
        assert_eq!(scratch_repo, scratch.as_str());
    }
    let listed = f.store.projects(f.ws(), f.key()).unwrap();
    let row = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == scratch.as_str())
        .unwrap()
        .clone();
    assert_eq!(
        (row["role"].clone(), row["scratch_of"].clone()),
        (json!("pr_scratch"), json!(project))
    );
    // A project without a link keeps its own scope.
    f.store
        .read(|db| {
            assert_eq!(graph_projection::identity_scope(db, &scratch)?, project);
            assert_eq!(graph_projection::identity_scope(db, &project)?, project);
            Ok(())
        })
        .unwrap();
}

const PYPROJECT_ALIB: &str = "[project]\nname = \"alib\"\nversion = \"1.2.0\"\n";
const PYPROJECT_BAPP: &str =
    "[project]\nname = \"bapp\"\nversion = \"0.1.0\"\ndependencies = [\"alib>=1.0\"]\n";

/// A folder synced as a project inside another synced project's folder owns
/// the packages under it: the outer project stops exporting
/// them, so an application's imports of the package resolve instead of
/// being ambiguous between the two.
#[test]
fn a_nested_project_owns_the_packages_under_its_folder() {
    let mut f = Fixture::new();
    let mono = f.folder("mono");
    f.write(
        &mono,
        &[
            ("lib/pyproject.toml", PYPROJECT_ALIB),
            ("lib/alib/__init__.py", ""),
            (
                "lib/alib/core.py",
                "def process(items):\n    return list(items)\n",
            ),
        ],
    );
    let app = f.folder("app");
    f.write(
        &app,
        &[
            ("pyproject.toml", PYPROJECT_BAPP),
            ("bapp/__init__.py", ""),
            (
                "bapp/ingest.py",
                "from alib.core import process\n\ndef ingest(rows):\n    return process(rows)\n",
            ),
        ],
    );
    let outer = f.sync(&mono, None);
    let inner = f.sync(&mono.join("lib"), None);
    let application = f.sync(&app, None);
    f.drain();
    let edges = |repository: &str| {
        f.store
            .graph_cross_edges(f.ws(), f.key(), repository)
            .unwrap()
    };
    let into_inner = edges(&inner)["incoming"].as_array().unwrap().clone();
    assert!(
        into_inner
            .iter()
            .any(|e| e["source_repository_id"] == application.as_str()),
        "{into_inner:#?}"
    );
    assert_eq!(edges(&outer)["incoming"], json!([]));
    let reasons: Vec<String> = f
        .store
        .read(|db| {
            Ok(graphicle_runtime::store::rows(
                db,
                "SELECT reason FROM gf_cross_unresolved WHERE source_repository_id=? AND valid_to_episode IS NULL",
                [&application],
            )?
            .iter()
            .map(|row| s(row, "reason").to_string())
            .collect())
        })
        .unwrap();
    assert!(
        !reasons.iter().any(|reason| reason.contains("ambiguous")),
        "{reasons:?}"
    );
}

/// An earlier `graphicle pr` run's `git:` copy of a library and the PR
/// scratch project a new run keeps for the library's folder project both
/// export the library; the newer one does, so an application's imports of
/// it are not ambiguous.
#[test]
fn the_newest_pr_scratch_copy_of_a_package_exports_it() {
    let mut f = Fixture::new();
    let library = [
        ("pyproject.toml", PYPROJECT_ALIB),
        ("alib/__init__.py", ""),
        (
            "alib/core.py",
            "def process(items):\n    return list(items)\n",
        ),
    ];
    let legacy = s(
        &f.store
            .register(
                f.ws(),
                f.key(),
                &json!({"project_key":format!("git:{}", "cd".repeat(32)),"name":"alib (pr)"}),
            )
            .unwrap(),
        "id",
    )
    .to_string();
    f.store.mark_pr_scratch(f.ws(), f.key(), &legacy).unwrap();
    f.commit(
        &legacy,
        &library,
        "aaa1110000000000000000000000000000000000",
    );
    // The library's folder project, registered by a later run but never
    // synced, and its PR scratch project.
    let checkout = f.root.join("alib-checkout");
    std::fs::create_dir_all(&checkout).unwrap();
    let (project, created) = f
        .store
        .folder_project(f.ws(), f.key(), &checkout, None, None, "alib-checkout")
        .unwrap();
    assert!(created);
    let scratch = s(
        &f.store.pr_scratch_for(f.ws(), f.key(), &project).unwrap(),
        "id",
    )
    .to_string();
    f.commit(
        &scratch,
        &library,
        "bbb2220000000000000000000000000000000000",
    );
    let app = f.folder("app");
    f.write(
        &app,
        &[
            ("pyproject.toml", PYPROJECT_BAPP),
            ("bapp/__init__.py", ""),
            (
                "bapp/ingest.py",
                "from alib.core import process\n\ndef ingest(rows):\n    return process(rows)\n",
            ),
        ],
    );
    let application = f.sync(&app, None);
    f.drain();
    let incoming = |repository: &str| {
        f.store
            .graph_cross_edges(f.ws(), f.key(), repository)
            .unwrap()["incoming"]
            .clone()
    };
    assert!(
        incoming(&scratch)
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["source_repository_id"] == application.as_str()),
        "{:#}",
        incoming(&scratch)
    );
    assert_eq!(incoming(&legacy), json!([]));
}

/// Rows of the Python host's workspace-source links are
/// no longer read into a project's sources.
#[test]
fn legacy_workspace_source_links_are_not_read() {
    let mut f = Fixture::new();
    let a = f.folder("a");
    let project = f.sync(&a, None);
    let db = f.store.connect().unwrap();
    db.execute(
        "INSERT INTO sources(id,workspace_id,name,role,content,sha256,created_at,version) VALUES('legacy-1',?1,'legacy.md','note','Legacy text.',?2,'2026-01-01',1)",
        rusqlite::params![f.ws(), graphicle_memory::text::sha256(b"Legacy text.")],
    )
    .unwrap();
    db.execute(
        "INSERT INTO source_heads(workspace_id,name,role,source_id) VALUES(?1,'legacy.md','note','legacy-1')",
        [f.ws()],
    )
    .unwrap();
    db.execute(
        "INSERT INTO repo_evidence_links(id,repository_id,kind,resource_id,follow_head,created_at) VALUES('link-1',?1,'source','legacy-1',1,'2026-01-01')",
        [&project],
    )
    .unwrap();
    let ids: Vec<String> = f
        .store
        .authorized_read(f.ws(), f.key(), "read", |db, p| {
            let repository = graphicle_runtime::store::repo(db, p, &project)?;
            Ok(memory::documents(db, &repository, p)?
                .iter()
                .map(|d| s(d, "id").to_string())
                .collect())
        })
        .unwrap();
    assert_eq!(ids, vec!["file:notes.txt".to_string()]);
}

// --- Project identity: more cases ------------------------------------------

/// Run `command` without input; its whole output, whatever the exit status.
fn output(mut command: Command) -> std::process::Output {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::fork_guard::spawn(&mut command)
        .unwrap()
        .wait_with_output()
        .unwrap()
}

/// A Claude Code transcript with one completed turn, run in `cwd`.
fn transcript(cwd: &Path) -> String {
    let record = |kind: &str, at: &str, message: Value| {
        json!({"type":kind,"sessionId":"0d9e8f7a-1111-4222-8333-944445555666","cwd":cwd.to_string_lossy(),
            "gitBranch":"main","timestamp":at,"message":message})
        .to_string()
    };
    [
        record(
            "user",
            "2026-10-13T10:00:00Z",
            json!({"role":"user","content":"Why does gamma return thirty?"}),
        ),
        record(
            "assistant",
            "2026-10-13T10:00:05Z",
            json!({"role":"assistant","content":[{"type":"text","text":"The head commit changed it."}]}),
        ),
    ]
    .join("\n")
        + "\n"
}

/// `graphicle call` sends workspace operations, which take no project, with
/// whatever `--project` says or with none at all, as the edition oracle
/// does (`call register --project oracle-unset`); project operations still
/// check the value they are given.
#[test]
fn call_sends_workspace_operations_without_a_project() {
    let f = Fixture::new();
    let arguments = f.root.join("register.json");
    let call = |args: &[&str]| {
        let mut command = f.binary(&f.root);
        command.arg("call").args(args);
        command
    };
    std::fs::write(
        &arguments,
        json!({"project_key":"oracle:empty","name":"Oracle empty project"}).to_string(),
    )
    .unwrap();
    let registered: Value = serde_json::from_str(&run(
        call(&[
            "register",
            "--project",
            "oracle-unset",
            "--arguments",
            arguments.to_str().unwrap(),
        ]),
        "",
    ))
    .unwrap();
    let id = s(&registered, "id").to_string();
    assert_eq!(id.len(), 32, "{registered}");
    // No --project, no registered folder and no saved default.
    assert_eq!(f.session.project, None);
    std::fs::write(
        &arguments,
        json!({"project_key":"oracle:second","name":"Second"}).to_string(),
    )
    .unwrap();
    let second: Value = serde_json::from_str(&run(
        call(&["register", "--arguments", arguments.to_str().unwrap()]),
        "",
    ))
    .unwrap();
    assert_ne!(s(&second, "id"), id);
    let info: Value = serde_json::from_str(&run(call(&["build.info"]), "")).unwrap();
    assert!(info["edition"].is_string(), "{info}");
    // A project operation checks the value: unknown here.
    let refused = output(call(&["status", "--project", "oracle-unset"]));
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("Repository not found"),
        "{refused:?}"
    );
    let refused = output(call(&["status"]));
    assert!(!refused.status.success(), "no project, no status");
    // An id or a key names the project.
    for value in [id.as_str(), "oracle:empty"] {
        let status: Value =
            serde_json::from_str(&run(call(&["status", "--project", value]), "")).unwrap();
        assert_eq!(status["repository"]["id"], id.as_str(), "{value}");
    }
}

/// Folder aliases are where a project's files are, not what it holds: two
/// stores with the same content synced from different folders have the
/// same `deletion.state` (the contract the edition oracle and forgetting
/// verification compare by).
#[test]
fn folder_locations_do_not_change_the_deletion_state() {
    let state = |label: &str| -> (Value, i64) {
        let mut f = Fixture::new();
        let folder = f.root.join(label);
        std::fs::create_dir_all(&folder).unwrap();
        f.write(
            &folder,
            &[
                ("notes.txt", "Shared notes.\n"),
                ("pkg/core.py", "def alpha():\n    return 1\n"),
            ],
        );
        let project = s(
            &f.store
                .register(
                    f.ws(),
                    f.key(),
                    &json!({"project_key":"oracle:fixture","name":"Oracle fixture project"}),
                )
                .unwrap(),
            "id",
        )
        .to_string();
        assert_eq!(f.sync(&folder, Some(&project)), project);
        f.store
            .read(|db| {
                let aliases: i64 =
                    db.query_row("SELECT count(*) FROM repo_folders", [], |r| r.get(0))?;
                Ok((
                    graphicle_runtime::deletion::state(db, f.ws(), &project)?,
                    aliases,
                ))
            })
            .unwrap()
    };
    let (first, first_aliases) = state("first-place");
    let (second, second_aliases) = state("elsewhere/second-place");
    assert_eq!((first_aliases, second_aliases), (1, 1));
    assert_eq!(first["tables"], second["tables"]);
    assert_eq!(first["digest"], second["digest"]);
    assert!(first["tables"].get("repo_folders").is_none(), "{first:#}");
    assert_eq!(first["excluded"]["locations"], json!(["repo_folders"]));
}

/// A project registered for a checkout and never synced (as `graphicle pr`
/// registers one) is no folder's project: commands, the capture hooks and
/// `sessions import` pass over it, until a sync of the folder fills that
/// same project.
#[test]
fn a_registered_project_is_no_folders_project_until_a_sync_fills_it() {
    let mut f = Fixture::new();
    let checkout = f.folder("checkout");
    let (project, created) = f
        .store
        .folder_project(f.ws(), f.key(), &checkout, None, None, "checkout")
        .unwrap();
    assert!(created);
    assert_eq!(f.at(&checkout), None);
    assert_eq!(
        capture::project_for_cwd(&f.store, &f.session, &checkout).unwrap(),
        None
    );
    assert!(f.resolve(None, &checkout, None).is_err());
    let file = f.root.join("session.jsonl");
    std::fs::write(&file, transcript(&checkout)).unwrap();
    let import = |f: &Fixture| -> Value {
        let mut command = f.binary(&f.root);
        command
            .args(["sessions", "import", "--client", "claude-code", "--path"])
            .arg(&file);
        serde_json::from_str(&run(command, "")).unwrap()
    };
    let skipped = import(&f);
    assert_eq!(
        (
            skipped["sessions"].clone(),
            skipped["skipped_outside_synced_projects"].clone()
        ),
        (json!(0), json!(1)),
        "{skipped:#}"
    );
    // The sync fills the registered project and records the folder.
    assert_eq!(f.sync(&checkout, None), project);
    assert_eq!(f.at(&checkout), Some(project.clone()));
    let captured = import(&f);
    assert_eq!(captured["sessions"], 1, "{captured:#}");
    let connector = s(&captured["captured"][0], "connector_id").to_string();
    let owner: String = f
        .store
        .read(|db| {
            Ok(db.query_row(
                "SELECT repository_id FROM am_connectors WHERE id=?",
                [&connector],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(owner, project);
}

/// `sync <folder> --project <path>` needs the path to be a registered
/// folder itself: a subfolder never names its ancestor's project, whose
/// files the sync would replace. Other commands resolve a path as the
/// working directory resolves.
#[test]
fn a_synced_folder_named_by_path_must_be_registered_itself() {
    let mut f = Fixture::new();
    let a = f.folder("a");
    let sub = f.folder("a/sub");
    let pa = f.sync(&a, None);
    let snapshot = |f: &Fixture| {
        f.store
            .project_status(f.ws(), f.key(), &json!({"repository_id": pa}))
            .unwrap()["repository"]["snapshot"]
            .clone()
    };
    let before = snapshot(&f);
    let refused =
        local::sync(&f.store, &mut f.session, &sub, Some(sub.to_str().unwrap())).unwrap_err();
    assert!(refused.message.contains("itself"), "{}", refused.message);
    assert_eq!(snapshot(&f), before, "the parent project kept its files");
    assert_eq!(f.at(&sub), Some(pa.clone()), "sub is still a's");
    assert_eq!(f.resolve(Some("sub"), &a, None).unwrap().id, pa);
    // The registered folder itself names its project.
    let psub = f.sync(&sub, None);
    let again = local::sync(&f.store, &mut f.session, &sub, Some(sub.to_str().unwrap())).unwrap();
    assert_eq!(s(&again, "repository_id"), psub);
}

/// The nearest registered folder decides even when this access key may not
/// read its project: commands refuse, the hooks pass over it, and neither
/// falls through to the project of a folder above it.
#[test]
fn an_unreadable_folder_project_is_refused_not_passed_over() {
    let mut f = Fixture::new();
    let a = f.folder("a");
    let sub = f.folder("a/sub");
    let pa = f.sync(&a, None);
    let psub = f.sync(&sub, None);
    std::fs::create_dir_all(sub.join("deep")).unwrap();
    let reader = f
        .store
        .grant(
            f.ws(),
            f.key(),
            &json!({"role":"reader","label":"restricted"}),
        )
        .unwrap();
    let token = s(&reader, "capability").to_string();
    f.store
        .graph_access(
            f.ws(),
            f.key(),
            &json!({"repository_id": psub, "credential_id": reader["id"], "state":"revoked"}),
        )
        .unwrap();
    let resolve = |requested: Option<&str>, cwd: &Path| {
        f.store
            .resolve_project(f.ws(), &token, requested, Some(cwd), Some(&pa))
    };
    let refused = resolve(None, &sub.join("deep")).unwrap_err();
    assert!(
        refused.message.contains("not readable"),
        "{}",
        refused.message
    );
    assert!(resolve(Some(sub.to_str().unwrap()), &f.root).is_err());
    assert_eq!(
        f.store.project_for_path(f.ws(), &token, &sub).unwrap(),
        None,
        "the hooks capture nothing there, not even into a's project"
    );
    assert_eq!(resolve(None, &a).unwrap().id, pa);
    assert_eq!(f.at(&sub.join("deep")), Some(psub), "the owner reads it");
}

/// A PR scratch project holds copies of its folder project's code: a key
/// revoked for the folder project cannot read the copy, and deleting the
/// folder project deletes the copy too, each with a verified receipt.
#[test]
fn a_scratch_project_follows_its_folder_projects_access_and_deletion() {
    let mut f = Fixture::new();
    let a = f.folder("a");
    f.write(&a, &[("pkg/core.py", "def alpha():\n    return 1\n")]);
    let project = f.sync(&a, None);
    let scratch = s(
        &f.store.pr_scratch_for(f.ws(), f.key(), &project).unwrap(),
        "id",
    )
    .to_string();
    f.commit(
        &scratch,
        &[("pkg/core.py", "def alpha():\n    return 2\n")],
        "aaa1110000000000000000000000000000000000",
    );
    f.drain();
    let reader = f
        .store
        .grant(
            f.ws(),
            f.key(),
            &json!({"role":"reader","label":"restricted"}),
        )
        .unwrap();
    let token = s(&reader, "capability").to_string();
    assert!(f.store.graph_episodes(f.ws(), &token, &scratch).is_ok());
    f.store
        .graph_access(
            f.ws(),
            f.key(),
            &json!({"repository_id": project, "credential_id": reader["id"], "state":"revoked"}),
        )
        .unwrap();
    assert!(f.store.graph_episodes(f.ws(), &token, &scratch).is_err());
    let visible = f.store.projects(f.ws(), &token).unwrap();
    assert!(
        visible
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["id"] != scratch.as_str()),
        "{visible:#}"
    );
    let snapshot = f
        .store
        .project_status(f.ws(), f.key(), &json!({"repository_id": project}))
        .unwrap()["repository"]["snapshot"]
        .clone();
    let deleted = f
        .store
        .maintain(
            f.ws(),
            f.key(),
            "project.delete",
            &json!({"repository_id": project, "expected_snapshot": snapshot}),
        )
        .unwrap();
    assert_eq!(deleted["receipt"]["verified_local"], true, "{deleted:#}");
    assert_eq!(
        deleted["receipt"]["removed"]["pr_scratch_projects"],
        json!([scratch])
    );
    let copies = deleted["pr_scratch_projects"].as_array().unwrap();
    assert_eq!(copies.len(), 1, "{deleted:#}");
    assert_eq!(copies[0]["repository_id"], scratch.as_str());
    assert_eq!(copies[0]["verified_local"], true, "{deleted:#}");
    // Both were projected: their episode labels went with them.
    for receipt in [&deleted["receipt"], &copies[0]] {
        assert!(
            receipt["removed"]["references"]["episode_labels"]
                .as_u64()
                .is_some_and(|n| n > 0),
            "{receipt:#}"
        );
    }
    let left: i64 = f
        .store
        .read(|db| Ok(db.query_row("SELECT count(*) FROM repo_projects", [], |r| r.get(0))?))
        .unwrap();
    assert_eq!(left, 0);
    let receipts = graphicle_runtime::deletion::list(&f.store, f.ws(), f.key()).unwrap();
    let chained: Vec<(Value, Value)> = receipts["receipts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["repository_id"].clone(), r["verified_local"].clone()))
        .collect();
    assert_eq!(
        chained,
        vec![(json!(project), json!(true)), (json!(scratch), json!(true))],
        "newest first: {receipts:#}"
    );
}

/// Only a project's current folder (the one it was last synced from) takes
/// part in the nested-project rule: once a nested project is synced from
/// another folder, its old folder's packages are the outer project's again.
#[test]
fn a_project_owns_packages_only_under_its_current_folder() {
    let mut f = Fixture::new();
    let mono = f.folder("mono");
    f.write(
        &mono,
        &[
            ("lib/pyproject.toml", PYPROJECT_ALIB),
            ("lib/alib/__init__.py", ""),
            (
                "lib/alib/core.py",
                "def process(items):\n    return list(items)\n",
            ),
        ],
    );
    let app = f.folder("app");
    f.write(
        &app,
        &[
            ("pyproject.toml", PYPROJECT_BAPP),
            ("bapp/__init__.py", ""),
            (
                "bapp/ingest.py",
                "from alib.core import process\n\ndef ingest(rows):\n    return process(rows)\n",
            ),
        ],
    );
    let outer = f.sync(&mono, None);
    let inner = f.sync(&mono.join("lib"), None);
    let application = f.sync(&app, None);
    f.drain();
    let from_app = |f: &Fixture, repository: &str| -> usize {
        f.store
            .graph_cross_edges(f.ws(), f.key(), repository)
            .unwrap()["incoming"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["source_repository_id"] == application.as_str())
            .count()
    };
    assert!(from_app(&f, &inner) > 0);
    assert_eq!(from_app(&f, &outer), 0);
    // The nested project now holds another folder's files; mono/lib stays
    // an alias of it, but no longer its current folder.
    let other = f.folder("other/blib");
    f.write(
        &other,
        &[
            (
                "pyproject.toml",
                "[project]\nname = \"blib\"\nversion = \"0.1.0\"\n",
            ),
            ("blib/__init__.py", ""),
            ("blib/core.py", "def other():\n    return 0\n"),
        ],
    );
    assert_eq!(f.sync(&other, Some(&inner)), inner);
    f.drain();
    assert_eq!(f.at(&mono.join("lib")), Some(inner.clone()));
    assert!(from_app(&f, &outer) > 0, "mono exports alib again");
    assert_eq!(from_app(&f, &inner), 0);
}

/// A sync that fails (here: too many files) records nothing for its folder:
/// the folder stays its project's, and the project named with `--project`
/// gains no folder.
#[test]
fn a_failed_sync_moves_no_alias() {
    let mut f = Fixture::new();
    let a = f.folder("a");
    let pa = f.sync(&a, None);
    let web = s(
        &f.store
            .register(
                f.ws(),
                f.key(),
                &json!({"project_key":"web-made","name":"Web notes"}),
            )
            .unwrap(),
        "id",
    )
    .to_string();
    // Past a configured project limit (100,000 files by default).
    for i in 0..11 {
        std::fs::write(a.join(format!("f{i}.txt")), "x\n").unwrap();
    }
    let limited = f
        .store
        .clone()
        .with_limits(graphicle_runtime::limits::Limits {
            project_files: 10,
            ..Default::default()
        });
    let failed = local::sync(&limited, &mut f.session, &a, Some(&web)).unwrap_err();
    assert!(
        failed.message.contains("(the limit is 10)"),
        "{}",
        failed.message
    );
    assert_eq!(f.at(&a), Some(pa.clone()));
    let aliases: Vec<String> = f
        .store
        .read(|db| {
            Ok(graphicle_runtime::store::rows(
                db,
                "SELECT repository_id FROM repo_folders ORDER BY repository_id",
                [],
            )?
            .iter()
            .map(|row| s(row, "repository_id").to_string())
            .collect())
        })
        .unwrap();
    assert_eq!(aliases, vec![pa]);
}

/// `connect codex` adds an entry named after the folder's project through
/// the Codex CLI (a stand-in here), naming the project's id.
#[cfg(unix)]
#[test]
fn connect_codex_adds_an_entry_named_for_the_project() {
    use std::os::unix::fs::PermissionsExt;
    let mut f = Fixture::new();
    let a = f.folder("a");
    let pa = f.sync(&a, None);
    let bin = f.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = f.root.join("codex-arguments.txt");
    let codex = bin.join("codex");
    std::fs::write(
        &codex,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n",
            log.to_string_lossy()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut command = f.binary(&a);
    command
        .env(
            "PATH",
            format!(
                "{}:{}",
                bin.to_string_lossy(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        // Codex's folder in the fixture: connect backs its config.toml up
        // and records it there.
        .env("CODEX_HOME", f.root.join("codex-home"))
        .args(["connect", "codex"]);
    let reply: Value = serde_json::from_str(&run(command, "")).unwrap();
    assert_eq!(reply["entry"], "graphicle-a", "{reply:#}");
    assert_eq!(reply["project"]["id"], pa.as_str());
    assert_eq!(reply["project"]["resolves"], "named in the entry");
    let arguments: Vec<String> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(arguments[..4], ["mcp", "add", "graphicle-a", "--"]);
    assert!(Path::new(&arguments[4]).is_absolute(), "{arguments:?}");
    assert_eq!(
        arguments[5..],
        [
            "--data-dir".to_string(),
            f.store
                .directory
                .canonical()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            "mcp".into(),
            "--project".into(),
            pa.clone()
        ]
    );
}
