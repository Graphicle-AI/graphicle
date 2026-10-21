//! `graphicle offboard`: one project's data leaves the store, its graph
//! cache and the data folder's backups, with a hash-chained receipt that
//! lists what was removed. A second project stays, in the store and in the
//! backups; nothing outside the data folder is touched; the plan changes
//! nothing, and only the plan that was shown is carried out.
// The symbolic and hard links in the backups folder are Unix fixtures.
#![cfg(unix)]
use graphicle_runtime::{
    deletion, jobs, local, offboard,
    store::{self, Store, s},
    terminal::Style,
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

/// Tables keyed by workspace rather than by project.
const WORKSPACE_TABLES: &[&str] = &["gf_dependencies", "gf_episode_labels"];

struct Fixture {
    _dir: TempDir,
    store: Store,
    session: local::Session,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        Self::open(false)
    }
    /// With `linked`, the store is opened through a symbolic link to the
    /// folder that holds it, so the store's path is not the data folder's
    /// resolved path: on macOS the temporary folder is such a path (/var is
    /// a link to /private/var).
    fn open(linked: bool) -> Self {
        let dir = TempDir::new().unwrap();
        let parent = if linked {
            std::fs::create_dir(dir.path().join("real")).unwrap();
            std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("link")).unwrap();
            dir.path().join("link")
        } else {
            dir.path().to_path_buf()
        };
        let store = Store::open(parent.join("store")).unwrap();
        let session = local::session(&store, true).unwrap();
        let root = dir.path().canonicalize().unwrap().join("work");
        std::fs::create_dir_all(&root).unwrap();
        Self {
            _dir: dir,
            store,
            session,
            root,
        }
    }
    /// A synced folder whose handbook carries `marker`.
    fn project(&mut self, name: &str, marker: &str) -> String {
        let folder = self.root.join(name);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("README.md"),
            format!("# Handbook\n\nThe {marker} handbook for this client.\n"),
        )
        .unwrap();
        std::fs::write(folder.join("app.py"), "def handler():\n    return 1\n").unwrap();
        let synced = local::sync(&self.store, &mut self.session, &folder, None).unwrap();
        s(&synced, "repository_id").to_string()
    }
    /// Run every queued job as a worker would.
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
    /// `project`'s state, without the tables keyed by workspace (their
    /// digests in a project's state cover every project's rows), and that
    /// project's own rows of those tables.
    fn state(&self, project: &str) -> (Value, Vec<String>) {
        let mut state = self
            .store
            .read(|db| deletion::state(db, &self.session.workspace, project))
            .unwrap();
        let tables = state["tables"].as_object_mut().unwrap();
        for table in WORKSPACE_TABLES {
            tables.remove(*table);
        }
        state.as_object_mut().unwrap().remove("digest");
        let workspace = &self.session.workspace;
        let own = self
            .store
            .read(|db| {
                let prefix = format!("{project}:");
                let mut own = Vec::new();
                for row in store::rows(
                    db,
                    "SELECT * FROM gf_dependencies WHERE workspace=?1 AND (substr(source_id,1,length(?2))=?2 OR substr(dependent_id,1,length(?2))=?2) ORDER BY source_id,dependent_id",
                    rusqlite::params![workspace, prefix],
                )? {
                    own.push(row.to_string());
                }
                for row in store::rows(
                    db,
                    "SELECT * FROM gf_episode_labels WHERE workspace=?1 AND repo=?2",
                    rusqlite::params![workspace, project],
                )? {
                    own.push(row.to_string());
                }
                Ok(own)
            })
            .unwrap();
        assert!(!own.is_empty(), "{project} has dependency entries");
        (state, own)
    }
    fn exists(&self, project: &str) -> bool {
        self.store
            .read(|db| {
                Ok(db
                    .query_row(
                        "SELECT count(*) FROM repo_projects WHERE id=?",
                        [project],
                        |r| r.get::<_, i64>(0),
                    )
                    .unwrap()
                    == 1)
            })
            .unwrap()
    }
}

fn sha(path: &Path) -> String {
    graphicle_memory::text::sha256(&std::fs::read(path).unwrap())
}

fn holds(path: &Path, text: &str) -> bool {
    let bytes = std::fs::read(path).unwrap();
    bytes
        .windows(text.len())
        .any(|window| window == text.as_bytes())
}

/// The projects a store file holds.
fn projects(path: &Path) -> Vec<String> {
    let db = rusqlite::Connection::open(path).unwrap();
    let mut statement = db
        .prepare("SELECT id FROM repo_projects ORDER BY id")
        .unwrap();
    statement
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn paths(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .map(|entry| s(entry, "path").to_string())
        .collect()
}

#[test]
fn offboarding_removes_one_project_with_its_backups_and_keeps_the_other() {
    offboard_one_then_the_other(false);
}

/// The same with the data folder reached through a symbolic link, as the
/// temporary folder is on macOS: what is inside the data folder and its
/// backups folder is decided on resolved paths, so the plan, its hash and
/// what is removed are the same.
#[test]
fn offboarding_through_a_link_to_the_data_folder_is_the_same() {
    offboard_one_then_the_other(true);
}

fn offboard_one_then_the_other(linked: bool) {
    crate::edition_setup();
    let mut f = Fixture::open(linked);
    let a = f.project("client-a", "zebracorn-alpha-7731");
    let b = f.project("client-b", "quokka-beta-5512");
    f.drain();
    let data = f.store.directory.canonicalize().unwrap();
    if linked {
        assert_ne!(data, *f.store.directory);
    }
    for id in [&a, &b] {
        let cache = data.join("gf-cache").join(id);
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("marker"), id.as_bytes()).unwrap();
    }
    // A backup in the data folder (`graphicle backup` without a path), and
    // one written elsewhere and linked into the backups folder.
    let backup = f
        .store
        .backup(&offboard::backup_path(&f.store).unwrap())
        .unwrap();
    // The backup's path is spelled as the store's path is (through the
    // link, or under /var on macOS): it resolves inside the backups folder.
    let kept = PathBuf::from(s(&backup, "path"));
    assert!(
        kept.canonicalize()
            .unwrap()
            .starts_with(data.join(offboard::BACKUPS)),
        "{kept:?}"
    );
    let outside = f.root.join("elsewhere.sqlite3");
    f.store.backup(&outside).unwrap();
    let backups = data.join(offboard::BACKUPS);
    std::os::unix::fs::symlink(&outside, backups.join("linked.sqlite3")).unwrap();
    // One written elsewhere with a second name in the backups folder (a hard
    // link): changing it would change the file outside too.
    let hard = f.root.join("hard.sqlite3");
    f.store.backup(&hard).unwrap();
    std::fs::hard_link(&hard, backups.join("hard.sqlite3")).unwrap();
    std::fs::write(backups.join("notes.txt"), "not a store").unwrap();
    let name = format!(
        "{}/{}",
        offboard::BACKUPS,
        kept.file_name().unwrap().to_str().unwrap()
    );
    let (kept_sha, outside_sha, hard_sha) = (sha(&kept), sha(&outside), sha(&hard));
    assert!(holds(&kept, "zebracorn-alpha-7731") && holds(&kept, "quokka-beta-5512"));
    let b_state = f.state(&b);

    // The plan changes nothing and lists what would go.
    let plan = offboard::plan(&f.store, &f.session, &a).unwrap();
    assert_eq!(plan["dry_run"], true);
    assert_eq!(plan["blocked"], false, "{plan:#}");
    assert_eq!(plan["data_dir"], json!(data), "{plan:#}");
    assert_eq!(plan["project"]["id"], json!(a));
    assert!(
        plan["store_rows"]["repo_objects"].as_i64().unwrap() > 0,
        "{plan:#}"
    );
    assert_eq!(paths(&plan["backups"]), vec![name.clone()], "{plan:#}");
    assert_eq!(plan["backups"][0]["other_projects"], 1);
    assert_eq!(plan["backups"][0]["sha256"], json!(kept_sha));
    assert!(paths(&plan["graph_cache"]).contains(&format!("gf-cache/{a}")));
    assert_eq!(
        paths(&plan["refused"]),
        vec![
            format!("{}/hard.sqlite3", offboard::BACKUPS),
            format!("{}/linked.sqlite3", offboard::BACKUPS),
        ],
        "{plan:#}"
    );
    assert!(s(&plan["refused"][0], "reason").contains("hard link"));
    assert_eq!(
        paths(&plan["skipped"]),
        vec![format!("{}/notes.txt", offboard::BACKUPS)]
    );
    let hash = s(&plan, "plan_hash").to_string();
    assert_eq!(
        s(&plan, "next"),
        format!("graphicle offboard --project {a} --apply {hash}")
    );
    assert_eq!(sha(&kept), kept_sha);
    assert!(f.exists(&a) && data.join("gf-cache").join(&a).exists());

    // The command line shows the same plan, and changes nothing either.
    let mut command = Command::new(env!("CARGO_BIN_EXE_graphicle"));
    command
        .current_dir(&f.root)
        .arg("--data-dir")
        .arg(f.store.directory.as_path())
        .args(["offboard", "--project", &a, "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = crate::fork_guard::spawn(&mut command)
        .unwrap()
        .wait_with_output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let shown: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(shown["plan_hash"], json!(hash));
    assert_eq!(sha(&kept), kept_sha);
    assert!(f.exists(&a));

    // A hash that is not the plan's changes nothing.
    let refused = offboard::apply(&f.store, &f.session, &a, "not-the-plan").unwrap_err();
    assert!(
        refused.message.contains("plan changed"),
        "{}",
        refused.message
    );
    assert_eq!(sha(&kept), kept_sha);
    assert!(f.exists(&a));

    // Applied: the project leaves the store, its cache and the backup. The
    // two links in the backups folder were refused and never changed, so
    // they may still hold it: the offboarding is not verified, and says why.
    let done = offboard::apply(&f.store, &f.session, &a, &hash).unwrap();
    assert_eq!(done["offboarded"], true);
    assert_eq!(done["verified"], false, "{done:#}");
    assert_eq!(paths(&done["refused"]).len(), 2, "{done:#}");
    let shown = offboard::text(&done, Style::plain());
    assert!(shown.contains("NOT fully verified"), "{shown}");
    assert!(
        shown.contains("2 refused paths were not changed and may still hold the project"),
        "{shown}"
    );
    let receipt = &done["receipt"];
    assert_eq!(receipt["operation"], "project.delete");
    assert_eq!(receipt["verified_local"], true, "{receipt:#}");
    let listed = &receipt["removed"]["offboarding"];
    assert_eq!(listed["plan_hash"], json!(hash));
    assert_eq!(paths(&listed["backups"]), vec![name.clone()]);
    assert_eq!(listed["backups"][0]["sha256_before"], json!(kept_sha));
    assert_eq!(listed["backups"][0]["sha256_after"], json!(sha(&kept)));
    assert_eq!(listed["backups"][0]["other_projects_kept"], 1);
    assert_eq!(listed["backups"][0]["verified"], true);
    assert!(paths(&listed["graph_cache"]).contains(&format!("gf-cache/{a}")));
    let chain = deletion::list(&f.store, &f.session.workspace, &f.session.capability).unwrap();
    assert_eq!(chain["chain"]["intact"], true, "{chain:#}");
    assert_eq!(chain["chain"]["head"], receipt["digest"]);
    assert!(!f.exists(&a));
    assert!(!data.join("gf-cache").join(&a).exists());
    assert_eq!(projects(&kept), vec![b.clone()]);
    assert!(!holds(&kept, "zebracorn-alpha-7731"));

    // The other project stays: in the store, its cache and the backup.
    assert!(f.exists(&b));
    assert_eq!(f.state(&b), b_state);
    assert!(data.join("gf-cache").join(&b).join("marker").exists());
    assert!(holds(&kept, "quokka-beta-5512"));

    // Nothing outside the data folder was touched.
    assert_eq!(sha(&outside), outside_sha);
    assert!(projects(&outside).contains(&a));
    assert_eq!(sha(&hard), hard_sha);
    assert!(projects(&hard).contains(&a));
    assert!(std::fs::symlink_metadata(backups.join("linked.sqlite3")).is_ok());

    // The project is gone: there is nothing left to plan.
    assert!(offboard::plan(&f.store, &f.session, &a).is_err());

    // Without the links, nothing is refused: offboarding the other project
    // removes it from the store and the backup, verified.
    std::fs::remove_file(backups.join("linked.sqlite3")).unwrap();
    std::fs::remove_file(backups.join("hard.sqlite3")).unwrap();
    let plan = offboard::plan(&f.store, &f.session, &b).unwrap();
    assert_eq!(plan["refused"], json!([]), "{plan:#}");
    assert_eq!(paths(&plan["backups"]), vec![name.clone()], "{plan:#}");
    let done = offboard::apply(&f.store, &f.session, &b, s(&plan, "plan_hash")).unwrap();
    assert_eq!(done["verified"], true, "{done:#}");
    let shown = offboard::text(&done, Style::plain());
    assert!(shown.contains(", verified."), "{shown}");
    assert!(!shown.contains("may still hold"), "{shown}");
    assert!(!f.exists(&b));
    assert_eq!(projects(&kept), Vec::<String>::new());
    assert!(!holds(&kept, "quokka-beta-5512"));
    // Outside the data folder, still nothing changed.
    assert_eq!(sha(&outside), outside_sha);
    assert_eq!(sha(&hard), hard_sha);
}

/// A `gf-cache` beside the backups that links out of the data folder, to a
/// folder named as the project, is refused and never followed: removing the
/// project from a backup changes the backup file only.
#[test]
fn offboarding_a_backup_never_follows_a_link_beside_it() {
    crate::edition_setup();
    let mut f = Fixture::new();
    let a = f.project("client-a", "zebracorn-alpha-7731");
    f.drain();
    let data = f.store.directory.canonicalize().unwrap();
    f.store
        .backup(&offboard::backup_path(&f.store).unwrap())
        .unwrap();
    let outside = f.root.join("elsewhere");
    std::fs::create_dir_all(outside.join(&a)).unwrap();
    std::fs::write(outside.join(&a).join("marker"), "outside").unwrap();
    std::os::unix::fs::symlink(&outside, data.join(offboard::BACKUPS).join("gf-cache")).unwrap();
    let plan = offboard::plan(&f.store, &f.session, &a).unwrap();
    assert_eq!(
        paths(&plan["refused"]),
        vec![format!("{}/gf-cache", offboard::BACKUPS)],
        "{plan:#}"
    );
    assert_eq!(paths(&plan["backups"]).len(), 1, "{plan:#}");
    let done = offboard::apply(&f.store, &f.session, &a, s(&plan, "plan_hash")).unwrap();
    assert_eq!(done["offboarded"], true);
    assert_eq!(done["backups"][0]["verified"], true, "{done:#}");
    assert_eq!(done["verified"], false, "{done:#}");
    assert!(!f.exists(&a));
    // The folder outside is as it was: nothing removed, no lock file added.
    let left: Vec<String> = std::fs::read_dir(outside.join(&a))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(left, vec!["marker".to_string()]);
}
