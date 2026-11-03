//! The early stop of a full rebuild (`update`, `watch`, the hooks' full
//! rebuilds) when nothing it reads changed since the last full rebuild, as
//! Graphify 0.9.100 stops ("No relevant changes", #4740).
//!
//! A full rebuild can find nothing to change: it extracts every file again,
//! finds the stored graph's topology and leaves graph.json, the report and
//! the labels as they are (`rebuild_inner`). Its output depends on what it
//! reads (never on the clock or the number of workers), so with the same
//! inputs and the same stored graph it finds nothing to change again. Such
//! a rebuild ends by recording what it read, in [`STATE_FILE`] in the
//! output folder. A rebuild that writes a new graph records nothing and
//! removes the record: its output depends on the stored graph it read, so
//! the next rebuild of the same inputs may still change it (on next.js
//! 16.4.0, two updates in a row with nothing changed added, then removed,
//! the same two nodes and moved 22 import edges). The record holds:
//! - the engine build (version, source hash, dependency lock, features, the
//!   program file and the grammar packs it may load);
//! - the settings that change what it writes: the roots, the options, the
//!   build configuration in the output folder, `.graphiclerc` and the
//!   engine's environment variables (not the worker count, the timing
//!   switches or the size cap: `NOT_OUTPUT_ENV`);
//! - the scan: every path detection classified, by kind;
//! - the content of each scanned file, through manifest.json, and of the
//!   scanned files the manifest has no hash for (files that failed);
//! - files outside the scan that extraction reads: `go.mod`, `go.work`,
//!   `package.json`, `jsconfig.json` and `tsconfig*.json` in the folders
//!   above the root, and in the root when an ignore rule hides them;
//! - graph.json's and manifest.json's size, times and inode.
//!
//! The record holds digests, counts, sizes, times and inodes, never a path
//! or a setting's value. The next full rebuild compares them before it
//! extracts anything and stops early when all are the same; it then does
//! what the full rebuild does when it finds nothing to change
//! (`watch::stop_early`). Anything it cannot prove the same (a missing or
//! unreadable record, a file it cannot read) takes the full rebuild: a false
//! "changed" only costs time. A rebuild that reads beyond these
//! (`--precise`: SCIP indexes, runtime traces, installed packages) never
//! stops early and records nothing.
use crate::{detect_manifest, paths, rcfile, serve_socket::FORWARDED_ENV, watch::RebuildOptions};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

/// The record of the last full rebuild, in the output folder.
pub const STATE_FILE: &str = ".graphicle_build_state.json";
/// The record's layout; another layout is never read as a match.
const SCHEMA: u32 = 1;
/// Settings that change how fast a rebuild runs, never what it writes (the
/// worker count among them: `cli/tests/same_graph.rs`), and the size cap on
/// reading graph.json, which [`before_scan`] applies as the full rebuild does.
const NOT_OUTPUT_ENV: &[&str] = &[
    "GRAPHICLE_MAX_GRAPH_BYTES",
    "GRAPHICLE_MAX_WORKERS",
    "GRAPHICLE_PRECISE_TIMING",
    "GRAPHICLE_PROFILE",
    "GRAPHICLE_QUERY_TIMING",
    "GRAPHICLE_WATCH_TIMING",
];
/// Files extraction reads that the scan may not cover: Go module paths and
/// JavaScript/TypeScript resolution settings.
fn config_name(name: &str) -> bool {
    matches!(
        name,
        "go.mod" | "go.work" | "package.json" | "jsconfig.json"
    ) || (name.starts_with("tsconfig") && name.ends_with(".json"))
}

fn digest(value: &impl Serialize) -> String {
    let bytes = serde_json::to_vec(value).expect("a digest's input serializes");
    format!("{:x}", Sha256::digest(&bytes))
}

/// A file's MD5, as the manifest records it, or a marker when it cannot be
/// read (a file that cannot be read is never proven unchanged).
fn content(path: &Path) -> String {
    match std::fs::metadata(detect_manifest::os_path(path)) {
        Ok(meta) if meta.is_file() => {
            let hash = detect_manifest::md5_file(path);
            if hash.is_empty() {
                "unreadable".into()
            } else {
                hash
            }
        }
        Ok(_) => "not a file".into(),
        Err(_) => "absent".into(),
    }
}

/// What tells a rewritten file from the one recorded: its size and times
/// (and on Unix its inode and status-change time, which every write and
/// replacement moves).
#[derive(Serialize, Deserialize, PartialEq, Eq, Clone, Debug, Default)]
pub struct FileStat {
    size: u64,
    modified_ns: u64,
    changed_ns: u64,
    inode: u64,
}
impl FileStat {
    pub fn of(path: &Path) -> Option<FileStat> {
        let meta = std::fs::metadata(path).ok()?;
        let ns = |time: std::io::Result<std::time::SystemTime>| {
            time.ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos() as u64)
        };
        #[cfg(unix)]
        let (changed_ns, inode) = {
            use std::os::unix::fs::MetadataExt;
            (
                (meta.ctime() as u64)
                    .saturating_mul(1_000_000_000)
                    .saturating_add(meta.ctime_nsec() as u64),
                meta.ino(),
            )
        };
        #[cfg(not(unix))]
        let (changed_ns, inode) = (0, 0);
        Some(FileStat {
            size: meta.len(),
            modified_ns: ns(meta.modified()),
            changed_ns,
            inode,
        })
    }
}

/// What a rebuild's output depends on besides the scanned files: the
/// engine build, the settings and the files above the root.
pub struct State {
    build: String,
    settings: String,
    above: String,
}
impl State {
    /// The state of a rebuild of `path` (resolved: `watch_root`) into `out`,
    /// or `None` when its output may depend on more than [`State`] and the
    /// scan record (`--precise`).
    pub fn current(
        path: &Path,
        watch_root: &Path,
        project_root: &Path,
        out: &Path,
        options: &RebuildOptions,
        excludes: &[String],
        gitignore: bool,
    ) -> Option<State> {
        if crate::extract::precise() {
            return None;
        }
        Some(State {
            build: build(),
            settings: settings(
                path,
                watch_root,
                project_root,
                out,
                options,
                excludes,
                gitignore,
            ),
            above: above(watch_root),
        })
    }
}

/// The engine build: what the rebuild's code is.
fn build() -> String {
    const LOCK: &str = include_str!("../Cargo.lock");
    let features = [
        ("bedrock", cfg!(feature = "bedrock")),
        ("chinese", cfg!(feature = "chinese")),
        ("grammar-commonlisp", cfg!(feature = "grammar-commonlisp")),
        ("grammar-dm", cfg!(feature = "grammar-dm")),
        ("grammar-elixir", cfg!(feature = "grammar-elixir")),
        ("grammar-fortran", cfg!(feature = "grammar-fortran")),
        ("grammar-julia", cfg!(feature = "grammar-julia")),
        ("grammar-objc", cfg!(feature = "grammar-objc")),
        ("grammar-ocaml", cfg!(feature = "grammar-ocaml")),
        ("grammar-powershell", cfg!(feature = "grammar-powershell")),
        ("grammar-verilog", cfg!(feature = "grammar-verilog")),
        ("grammar-zig", cfg!(feature = "grammar-zig")),
        ("postgres", cfg!(feature = "postgres")),
        ("precise-js", cfg!(feature = "precise-js")),
        ("transcription", cfg!(feature = "transcription")),
    ];
    // The program file itself: another build of the same source (another
    // compiler or profile) is another program.
    let program = std::env::current_exe()
        .ok()
        .and_then(|exe| FileStat::of(&exe).map(|stat| (exe.to_string_lossy().into_owned(), stat)));
    // Grammar packs a build without a linked grammar loads (`grammar_packs`):
    // the folders on their search path that hold any.
    let packs = crate::grammar_packs::search_path()
        .into_iter()
        .filter_map(|dir| {
            let mut entries = std::fs::read_dir(&dir)
                .ok()?
                .flatten()
                .map(|e| {
                    (
                        e.file_name().to_string_lossy().into_owned(),
                        FileStat::of(&e.path()),
                    )
                })
                .collect::<Vec<_>>();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            (!entries.is_empty()).then(|| (dir.to_string_lossy().into_owned(), entries))
        })
        .collect::<Vec<_>>();
    digest(&(
        SCHEMA,
        crate::VERSION,
        crate::UPSTREAM_VERSION,
        env!("GRAPHICLE_SNAPSHOT_CODE"),
        format!("{:x}", Sha256::digest(LOCK.as_bytes())),
        features,
        program,
        packs,
    ))
}

/// The settings that change what a rebuild writes.
fn settings(
    path: &Path,
    watch_root: &Path,
    project_root: &Path,
    out: &Path,
    options: &RebuildOptions,
    excludes: &[String],
    gitignore: bool,
) -> String {
    // HOME is not one: the grammar packs under it are part of the build, and
    // the ignore rules it holds show in the scan.
    let mut env = FORWARDED_ENV
        .iter()
        .filter(|name| !NOT_OUTPUT_ENV.contains(*name))
        .filter_map(|name| {
            let value = std::env::var_os(name)?;
            Some((*name, value.to_string_lossy().into_owned()))
        })
        .collect::<Vec<_>>();
    env.sort();
    let mut excludes = excludes.to_vec();
    excludes.sort();
    let spelled = |path: &Path| path.to_string_lossy().into_owned();
    digest(&(
        SCHEMA,
        [path, watch_root, project_root, out].map(spelled),
        paths::OUT_DIR.as_str(),
        (options.follow_symlinks, options.no_cluster),
        (excludes, gitignore),
        (
            crate::extractors::rst::enabled(),
            crate::extractors::routes::enabled(),
        ),
        content(&watch_root.join(rcfile::RC_FILENAME)),
        env,
    ))
}

/// The configuration files extraction may read in the folders above the
/// scan root, with their content.
fn above(watch_root: &Path) -> String {
    let rows = watch_root
        .ancestors()
        .skip(1)
        .map(|dir| {
            let listed = std::fs::read_dir(dir).map(|entries| {
                let mut found = entries
                    .flatten()
                    .filter(|e| config_name(&e.file_name().to_string_lossy()))
                    .map(|e| {
                        (
                            e.file_name().to_string_lossy().into_owned(),
                            content(&e.path()),
                        )
                    })
                    .collect::<Vec<_>>();
                found.sort();
                found
            });
            (dir.to_string_lossy().into_owned(), listed.ok())
        })
        .collect::<Vec<_>>();
    digest(&rows)
}

/// The paths of a detection result under `key` (a list of strings).
fn strings<'a>(detected: &'a Value, key: &str) -> impl Iterator<Item = &'a str> {
    detected[key]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
}

/// Every path detection classified, by kind: the scanned files of each kind
/// and the paths it left out (unclassified, ignored, sensitive, noise).
fn scan(detected: &Value) -> String {
    let mut files = detected["files"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(kind, list)| {
            let mut list = list
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>();
            list.sort_unstable();
            (kind.as_str(), list)
        })
        .collect::<Vec<_>>();
    files.sort();
    let rest = [
        "unclassified",
        "skipped_sensitive",
        "ignored",
        "pruned_noise_dirs",
        "walk_errors",
    ]
    .map(|key| {
        let mut list = strings(detected, key).collect::<Vec<_>>();
        list.sort_unstable();
        (key, list)
    });
    digest(&(files, rest))
}

/// The scanned files of a detection result, of every kind.
fn scanned(detected: &Value) -> impl Iterator<Item = &str> {
    detected["files"]
        .as_object()
        .into_iter()
        .flatten()
        .flat_map(|(_, list)| list.as_array().into_iter().flatten())
        .filter_map(Value::as_str)
}

/// A manifest row's content hash, when it has one.
fn row_hash(row: &Value) -> Option<&str> {
    row.get("ast_hash")
        .and_then(Value::as_str)
        .filter(|hash| !hash.is_empty())
}

/// The scanned files the manifest has no content hash for (files whose
/// extraction failed, which a full rebuild leaves unstamped).
fn unstamped<'a>(detected: &'a Value, manifest: &Value) -> BTreeSet<&'a str> {
    scanned(detected)
        .filter(|path| {
            manifest
                .get(paths::nfc(path).as_str())
                .and_then(row_hash)
                .is_none()
        })
        .collect()
}

/// The content of the files the manifest does not vouch for: the unstamped
/// scanned files, and configuration files an ignore rule hides from the scan.
fn inputs(detected: &Value, unstamped: &BTreeSet<&str>) -> String {
    let hidden = strings(detected, "ignored").chain(
        strings(detected, "skipped_sensitive").map(|entry| entry.split(" [").next().unwrap_or("")),
    );
    let mut files = unstamped
        .iter()
        .copied()
        .chain(hidden.filter(|path| {
            Path::new(path)
                .file_name()
                .is_some_and(|name| config_name(&name.to_string_lossy()))
        }))
        .collect::<Vec<_>>();
    files.sort_unstable();
    files.dedup();
    let rows = crate::par::map(&files, crate::par::io_workers(), |path| {
        (*path, content(Path::new(path)))
    });
    digest(&rows)
}

/// Whether a manifest row's file still has the content the row records:
/// `Some(true)` with the recorded modification time, `Some(false)` with
/// another one (a full rebuild would restamp the manifest), `None` when the
/// content changed or cannot be proven the same. A row without a content
/// hash is `Some(true)`: its file is checked by [`inputs`] if it is scanned.
fn row_unchanged(path: &str, row: &Value) -> Option<bool> {
    let Some(hash) = row_hash(row) else {
        return Some(true);
    };
    // A language a project declares for an extension is part of the row.
    let language = row.get("language").and_then(Value::as_str);
    if rcfile::cache_salt(path).as_deref() != language {
        return None;
    }
    let current = detect_manifest::mtime(Path::new(path)).ok()?;
    let recorded = if row["mtime"].is_object() {
        detect_manifest::numeric(&row["mtime"]["mtime"])
    } else {
        detect_manifest::numeric(&row["mtime"])
    };
    let same_time = recorded == Some(current);
    if same_time && !detect_manifest::mtime_may_hide_a_rewrite(current, row) {
        return Some(true);
    }
    (detect_manifest::md5_file(Path::new(path)) == hash).then_some(same_time)
}

/// The record a full rebuild leaves for the next one ([`STATE_FILE`]).
#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct Record {
    schema: u32,
    build: String,
    settings: String,
    above: String,
    scan: String,
    inputs: String,
    graph: FileStat,
    manifest: FileStat,
    nodes: usize,
    edges: usize,
}

/// Record a full rebuild of `detected` that found nothing to change and left
/// the outputs in `out` as they are now, with `nodes` and `edges` in its
/// graph. Nothing is recorded (and an older record is removed) when an
/// output it describes is missing or the record cannot be written: the next
/// rebuild is then a full one.
pub fn record(
    out: &Path,
    watch_root: &Path,
    state: Option<&State>,
    detected: &Value,
    nodes: usize,
    edges: usize,
) {
    let file = out.join(STATE_FILE);
    let made = state.and_then(|state| {
        let manifest_path = out.join("manifest.json");
        let manifest = detect_manifest::load_manifest(&manifest_path, Some(watch_root));
        let record = Record {
            schema: SCHEMA,
            build: state.build.clone(),
            settings: state.settings.clone(),
            above: state.above.clone(),
            scan: scan(detected),
            inputs: inputs(detected, &unstamped(detected, &manifest)),
            graph: FileStat::of(&out.join("graph.json"))?,
            manifest: FileStat::of(&manifest_path)?,
            nodes,
            edges,
        };
        let text = serde_json::to_string(&record).ok()?;
        paths::write_text_atomic(&file, &(text + "\n")).ok()
    });
    if made.is_none() {
        let _ = std::fs::remove_file(&file);
    }
}

/// Remove the record: a rebuild wrote a new graph, which the next full
/// rebuild may still change.
pub fn forget(out: &Path) {
    let _ = std::fs::remove_file(out.join(STATE_FILE));
}

/// A full rebuild whose record matched until the scan: the record and what
/// the manifest check found.
pub struct Pending {
    record: Record,
    manifest: Value,
    restamp: bool,
    converted: PathBuf,
}

/// What the early stop does besides leaving the outputs: restamp the
/// manifest when a file's modification time moved (its content did not),
/// as the full rebuild would, with the files that failed last time.
pub struct Unchanged {
    pub restamp: bool,
    pub failed: BTreeSet<String>,
    pub nodes: usize,
    pub edges: usize,
}

/// Why a full rebuild does not stop early: the first check that failed.
pub type Changed = &'static str;

/// The part of the check that needs no scan: the record exists, the build,
/// settings and files above the root are the same, graph.json and
/// manifest.json are the files it recorded, and every manifest row's file
/// has the content the row records. An error takes the full rebuild.
pub fn before_scan(out: &Path, watch_root: &Path, state: &State) -> Result<Pending, Changed> {
    let text = std::fs::read_to_string(out.join(STATE_FILE)).map_err(|_| "no record")?;
    // The rows' declared languages are compared with this root's (`.graphiclerc`).
    rcfile::activate_language_overrides(watch_root);
    let record = serde_json::from_str::<Record>(&text).map_err(|_| "record unreadable")?;
    let graph_path = out.join("graph.json");
    let manifest_path = out.join("manifest.json");
    let checks = [
        (record.schema == SCHEMA, "record layout"),
        (record.build == state.build, "engine build"),
        (record.settings == state.settings, "settings"),
        (record.above == state.above, "files above the root"),
        (
            FileStat::of(&graph_path).as_ref() == Some(&record.graph),
            "graph.json",
        ),
        (
            FileStat::of(&manifest_path).as_ref() == Some(&record.manifest),
            "manifest.json",
        ),
    ];
    if let Some((_, changed)) = checks.iter().find(|(same, _)| !same) {
        return Err(changed);
    }
    // The full rebuild reads graph.json under the size cap.
    crate::security::check_own_graph_size_cap(&graph_path).map_err(|_| "graph size cap")?;
    let manifest = detect_manifest::load_manifest(&manifest_path, Some(watch_root));
    let rows = manifest
        .as_object()
        .ok_or("manifest unreadable")?
        .iter()
        .collect::<Vec<_>>();
    let checked = crate::par::map(&rows, crate::par::io_workers(), |(path, row)| {
        row_unchanged(path, row)
    });
    let mut restamp = false;
    for unchanged in checked {
        restamp |= !unchanged.ok_or("file content")?;
    }
    Ok(Pending {
        record,
        manifest,
        restamp,
        converted: crate::extract_normalize::converted_dir(watch_root, None),
    })
}

impl Pending {
    /// The rest of the check, with the scan: the same paths classified the
    /// same way, and the same content for the files the manifest has no
    /// hash for. An error takes the full rebuild.
    pub fn after_scan(self, detected: &Value) -> Result<Unchanged, Changed> {
        let Pending {
            record,
            manifest,
            mut restamp,
            converted,
        } = self;
        if scan(detected) != record.scan {
            return Err("scanned files");
        }
        // The scan converts an Office document again when it is newer than
        // its Markdown sidecar: the sidecars' rows are checked once more.
        for path in scanned(detected).filter(|path| Path::new(path).starts_with(&converted)) {
            if let Some(row) = manifest.get(paths::nfc(path).as_str()) {
                restamp |= !row_unchanged(path, row).ok_or("converted document")?;
            }
        }
        let unstamped = unstamped(detected, &manifest);
        if inputs(detected, &unstamped) != record.inputs {
            return Err("unstamped or hidden files");
        }
        Ok(Unchanged {
            restamp,
            failed: unstamped.into_iter().map(str::to_string).collect(),
            nodes: record.nodes,
            edges: record.edges,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn configuration_files_by_name() {
        for name in [
            "go.mod",
            "go.work",
            "package.json",
            "jsconfig.json",
            "tsconfig.json",
            "tsconfig.base.json",
        ] {
            assert!(config_name(name), "{name}");
        }
        for name in [
            "go.sum",
            "package-lock.json",
            "tsconfig",
            "config.json",
            "Cargo.toml",
        ] {
            assert!(!config_name(name), "{name}");
        }
    }

    /// The scan record changes when a path changes kind or set, and not when
    /// the same paths come in another order.
    #[test]
    fn scan_records_every_classified_path() {
        let base = json!({"files":{"code":["/r/a.py","/r/b.py"],"document":["/r/README.md"]},
            "unclassified":["/r/LICENSE"],"ignored":["/r/build.py"],"skipped_sensitive":[],
            "pruned_noise_dirs":[],"walk_errors":[]});
        let reordered = json!({"files":{"document":["/r/README.md"],"code":["/r/b.py","/r/a.py"]},
            "unclassified":["/r/LICENSE"],"ignored":["/r/build.py"]});
        assert_eq!(scan(&base), scan(&reordered));
        let mut moved = base.clone();
        moved["files"]["code"] = json!(["/r/a.py"]);
        moved["ignored"] = json!(["/r/b.py", "/r/build.py"]);
        assert_ne!(scan(&base), scan(&moved));
        let mut renamed = base.clone();
        renamed["files"]["code"] = json!(["/r/a.py", "/r/c.py"]);
        assert_ne!(scan(&base), scan(&renamed));
        let mut kind = base.clone();
        kind["files"]["document"] = json!([]);
        kind["files"]["paper"] = json!(["/r/README.md"]);
        assert_ne!(scan(&base), scan(&kind));
    }

    /// The rows `stamp_manifest` writes vouch for every scanned file but the
    /// ones whose extraction failed, which the record hashes itself.
    #[test]
    fn a_stamped_scan_leaves_only_failed_files_unstamped() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let out = root.join(".graphicle");
        std::fs::create_dir_all(root.join("pkg")).unwrap();
        std::fs::create_dir_all(&out).unwrap();
        let (a, b) = (root.join("pkg/a.py"), root.join("b.py"));
        std::fs::write(&a, "x = 1\n").unwrap();
        std::fs::write(&b, "y = (\n").unwrap();
        let spell = |p: &Path| p.to_string_lossy().into_owned();
        let detected = json!({"files": {"code": [spell(&a), spell(&b)], "document": []}});
        let mut stamped = detected["files"].as_object().unwrap().clone();
        stamped["code"] = json!([spell(&a)]);
        let manifest_path = out.join("manifest.json");
        detect_manifest::save_manifest(
            &stamped,
            &manifest_path,
            "ast",
            &detect_manifest::ManifestOptions {
                root: Some(root.clone()),
                scan_corpus: Some(vec![spell(&a), spell(&b)]),
                clear_ast: Some(vec![spell(&b)]),
                ..Default::default()
            },
        )
        .unwrap();
        let manifest = detect_manifest::load_manifest(&manifest_path, Some(&root));
        let b = spell(&b);
        assert_eq!(
            unstamped(&detected, &manifest),
            BTreeSet::from([b.as_str()])
        );
    }

    /// A row vouches for its file while the file keeps the recorded content:
    /// with the recorded time it is not read; with another time its content
    /// decides; a row without a hash is left to the scan's check.
    #[test]
    fn rows_vouch_for_unchanged_content() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("a.py");
        std::fs::write(&file, "x = 1\n").unwrap();
        let path = file.to_string_lossy().into_owned();
        let mtime = detect_manifest::mtime(&file).unwrap();
        let hash = detect_manifest::md5_file(&file);
        // Seen long after it was written: outside the window where a
        // same-second rewrite can hide.
        let row =
            json!({"mtime": mtime, "seen": mtime + 100.0, "ast_hash": hash, "semantic_hash": ""});
        assert_eq!(row_unchanged(&path, &row), Some(true));
        let moved = json!({"mtime": mtime - 50.0, "seen": mtime + 100.0, "ast_hash": hash});
        assert_eq!(row_unchanged(&path, &moved), Some(false));
        let other =
            json!({"mtime": mtime - 50.0, "seen": mtime + 100.0, "ast_hash": "0".repeat(32)});
        assert_eq!(row_unchanged(&path, &other), None);
        assert_eq!(
            row_unchanged(&path, &json!({"mtime": mtime, "ast_hash": ""})),
            Some(true)
        );
        std::fs::remove_file(&file).unwrap();
        assert_eq!(row_unchanged(&path, &row), None);
    }
}
