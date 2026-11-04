//! The early stop of a full rebuild (`update`, a full `watch` rebuild, the
//! hooks' full rebuilds) when nothing it reads changed since a full rebuild
//! that found nothing to change, as Graphify 0.9.100 stops ("No relevant
//! changes", #4740).
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
//! the same two nodes and moved 22 import edges). The record holds digests
//! of:
//! - the engine build (version, source hash, dependency lock, features, the
//!   program file and the grammar packs it may load);
//! - the settings that change what it writes: the roots, the options, the
//!   build configuration in the output folder, `.graphiclerc`, HOME and the
//!   engine's environment variables (not the ones in `NOT_OUTPUT_ENV`);
//! - the scan: every path detection classified, by kind;
//! - the size, modification time and (on Unix) status-change time and inode
//!   of every scanned file and of each configuration file an ignore rule
//!   hides, taken after the scan, before extraction ([`Snapshot`]);
//! - the configuration files extraction reads in the folders above the root
//!   (`go.mod`, `go.work`, `package.json`, `jsconfig.json`, `tsconfig*.json`,
//!   `pnpm-workspace.yaml`, with their content) and whether those folders
//!   hold `.git`, `.hg` or `.svn`;
//! - graph.json's stats when the rebuild read it, and manifest.json's when
//!   it wrote it.
//!
//! The record holds digests, counts and file stats, never a path or a
//! setting's value. A rebuild records only when the snapshotted files,
//! graph.json and manifest.json still have the stats it saw, when none of
//! the snapshotted files changed within the time granularity before the
//! snapshot (two seconds by default, as the extraction cache trusts a
//! file's stats: a file system moves times in steps, so a later change
//! could keep them), and when no file failed to extract in a way a retry may
//! pass (a read error, a cancelled parse, a panic: the full rebuild retries
//! it). A failure the file's content decides (a parse failure, a size limit,
//! an encoding the extractor refuses: `extract::CONTENT_FAILURE`) repeats on
//! the same bytes, and the file's stats are recorded like any other's, so a
//! change to it takes the full rebuild. The next full rebuild
//! compares them before it extracts anything and stops early when all are
//! the same; it then does what the full rebuild does when it finds nothing
//! to change (`watch::stop_early`). The check trusts the recorded stats: a
//! file whose content changed while its size, times and inode stayed (a
//! change no file system makes on its own) is not seen. Anything else it
//! cannot prove the same takes the full rebuild. A rebuild that reads
//! beyond these never stops early and records nothing: `--precise` (SCIP
//! indexes, runtime traces, installed packages), and a JavaScript workspace
//! in a folder above the root (its member packages beside the project).
use crate::{detect_manifest, paths, rcfile, serve_socket::FORWARDED_ENV, watch::RebuildOptions};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::Path;

/// The record of the last full rebuild, in the output folder.
pub const STATE_FILE: &str = ".graphicle_build_state.json";
/// The record's layout; another layout is never read as a match.
const SCHEMA: u32 = 2;
/// Settings that change how fast or how verbosely a rebuild runs, never
/// what it writes: the worker count (`cli/tests/same_graph.rs`), the timing,
/// profile, log and socket switches, the resource limits, the hooks' own
/// settings and change lists (a full rebuild reads no change list), and the
/// size cap on reading graph.json, which [`before_scan`] applies as the full
/// rebuild does.
const NOT_OUTPUT_ENV: &[&str] = &[
    "GRAPHICLE_CHANGED",
    "GRAPHICLE_CHANGED_JSON",
    "GRAPHICLE_HOOK_NUDGE_CAP",
    "GRAPHICLE_HOOK_STRICT",
    "GRAPHICLE_HOOK_STRICT_TTL",
    "GRAPHICLE_MAX_GRAPH_BYTES",
    "GRAPHICLE_MAX_WORKERS",
    "GRAPHICLE_PRECISE_TIMING",
    "GRAPHICLE_PROFILE",
    "GRAPHICLE_QUERY_LOG",
    "GRAPHICLE_QUERY_LOG_DISABLE",
    "GRAPHICLE_QUERY_LOG_ENABLE",
    "GRAPHICLE_QUERY_LOG_RESPONSES",
    "GRAPHICLE_QUERY_TIMING",
    "GRAPHICLE_REBUILD_LOG",
    "GRAPHICLE_REBUILD_MEMORY_LIMIT_MB",
    "GRAPHICLE_REBUILD_TIMEOUT",
    "GRAPHICLE_SKIP_HOOK",
    "GRAPHICLE_SOCKET_DEBUG",
    "GRAPHICLE_SOCKET_TIMEOUT",
    "GRAPHICLE_WATCH_TIMING",
];
/// Files extraction reads that the scan may not cover: Go module paths and
/// JavaScript/TypeScript resolution settings.
fn config_name(name: &str) -> bool {
    matches!(
        name,
        "go.mod" | "go.work" | "package.json" | "jsconfig.json" | "pnpm-workspace.yaml"
    ) || (name.starts_with("tsconfig") && name.ends_with(".json"))
}
/// Version control folders: JavaScript resolution anchors a project without
/// a package.json at the nearest one (`find_js_project_anchor`).
fn vcs_name(name: &str) -> bool {
    matches!(name, ".git" | ".hg" | ".svn")
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
/// (and on Unix its inode and status-change time, which every write, every
/// change of the modification time and every replacement moves).
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
    /// or why its output may depend on more than [`State`] and the scan
    /// record: `--precise`, or a JavaScript workspace above the root.
    pub fn current(
        path: &Path,
        watch_root: &Path,
        project_root: &Path,
        out: &Path,
        options: &RebuildOptions,
        excludes: &[String],
        gitignore: bool,
    ) -> Result<State, Changed> {
        if crate::extract::precise() {
            return Err("--precise");
        }
        if workspace_above(watch_root) {
            return Err("a JavaScript workspace above the root");
        }
        Ok(State {
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

/// Whether a folder above the root may be a JavaScript workspace root
/// (`pnpm-workspace.yaml`, or a package.json with a `workspaces` key):
/// resolution then reads the workspace's member packages, which may lie
/// beside the project (`resolution_js::find_workspace_root`). The
/// package.json is read from disk, never from a running extraction's
/// cache, and any one that names `workspaces` counts.
fn workspace_above(watch_root: &Path) -> bool {
    watch_root.ancestors().skip(1).any(|dir| {
        dir.join("pnpm-workspace.yaml").exists()
            || std::fs::read(detect_manifest::os_path(&dir.join("package.json")))
                .is_ok_and(|bytes| bytes.windows(12).any(|w| w == b"\"workspaces\""))
    })
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
    // HOME bounds the search for a JavaScript project's package.json
    // (`resolution_js::find_js_project_anchor`).
    let mut env = FORWARDED_ENV
        .iter()
        .chain(&["HOME"])
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
/// scan root, with their content, and the version control folders there.
fn above(watch_root: &Path) -> String {
    let rows = watch_root
        .ancestors()
        .skip(1)
        .map(|dir| {
            let listed = std::fs::read_dir(dir).map(|entries| {
                let mut found = entries
                    .flatten()
                    .filter_map(|e| {
                        let name = e.file_name().to_string_lossy().into_owned();
                        if config_name(&name) {
                            Some((name, content(&e.path())))
                        } else if vcs_name(&name) {
                            Some((name, "present".into()))
                        } else {
                            None
                        }
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

/// The configuration files an ignore rule hides from the scan, which
/// extraction may still read.
fn hidden_configs(detected: &Value) -> impl Iterator<Item = &str> {
    strings(detected, "ignored")
        .chain(
            strings(detected, "skipped_sensitive")
                .map(|entry| entry.split(" [").next().unwrap_or("")),
        )
        .filter(|path| {
            Path::new(path)
                .file_name()
                .is_some_and(|name| config_name(&name.to_string_lossy()))
        })
}

/// The stats of the files a rebuild reads, taken after the scan and before
/// extraction: every scanned file and the configuration files an ignore
/// rule hides from the scan.
pub struct Snapshot {
    files: Vec<String>,
    stats: Vec<Option<FileStat>>,
    digest: String,
    /// When the stats were taken, and the time a file system may take to
    /// move a file's times (`GRAPHICLE_MTIME_GRANULARITY_MS`, as the
    /// extraction cache reads it).
    taken_ns: u64,
    granularity_ns: u64,
}
impl Snapshot {
    pub fn take(detected: &Value) -> Snapshot {
        let granularity = crate::cache::mtime_granularity(
            std::env::var("GRAPHICLE_MTIME_GRANULARITY_MS")
                .ok()
                .as_deref(),
        );
        Snapshot::with_granularity(detected, granularity.max(0) as u64)
    }
    fn with_granularity(detected: &Value, granularity_ns: u64) -> Snapshot {
        let mut files = scanned(detected)
            .chain(hidden_configs(detected))
            .map(str::to_string)
            .collect::<Vec<_>>();
        files.sort_unstable();
        files.dedup();
        let taken_ns = crate::cache::clock_ns().max(0) as u64;
        let stats = stats_of(&files);
        let digest = rows_digest(&files, &stats);
        Snapshot {
            files,
            stats,
            digest,
            taken_ns,
            granularity_ns,
        }
    }
    /// Whether every file still has the stats it had.
    fn unchanged(&self) -> bool {
        rows_digest(&self.files, &stats_of(&self.files)) == self.digest
    }
    /// Whether a file changed so shortly before the stats were taken that a
    /// later change could leave its size and times as they were (a file
    /// system moves times in steps, up to the granularity): its stats then
    /// prove nothing, as the extraction cache treats them (`stat_fresh`).
    fn recent(&self) -> bool {
        self.stats.iter().flatten().any(|stat| {
            stat.modified_ns
                .max(stat.changed_ns)
                .saturating_add(self.granularity_ns)
                > self.taken_ns
        })
    }
}
fn stats_of(files: &[String]) -> Vec<Option<FileStat>> {
    crate::par::map(files, crate::par::io_workers(), |path| {
        FileStat::of(Path::new(path))
    })
}
fn rows_digest(files: &[String], stats: &[Option<FileStat>]) -> String {
    digest(&files.iter().zip(stats).collect::<Vec<_>>())
}

/// The record a full rebuild leaves for the next one ([`STATE_FILE`]).
#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct Record {
    schema: u32,
    build: String,
    settings: String,
    above: String,
    scan: String,
    files: String,
    graph: FileStat,
    manifest: FileStat,
    nodes: usize,
    edges: usize,
}

/// What a full rebuild saw while it ran, for its record.
pub struct Seen<'a> {
    /// The scanned files' stats before extraction.
    pub snapshot: &'a Snapshot,
    /// graph.json's stats when the rebuild read or checked it.
    pub graph: Option<FileStat>,
    /// manifest.json's stats when the rebuild wrote or checked it.
    pub manifest: Option<FileStat>,
    /// Whether a file failed to extract in a way a retry may pass: any
    /// failure its content does not decide (`extract::CONTENT_FAILURE`).
    pub retryable: bool,
}

/// Record a full rebuild of `detected` that found nothing to change, with
/// `nodes` and `edges` in its graph, when what it saw still holds: no file
/// failed in a way a retry may pass, no snapshotted file changed shortly
/// before the snapshot and each still has its stats, and graph.json and
/// manifest.json are the files the rebuild read and wrote. Otherwise, or
/// when the record cannot be written, any record is removed (the next
/// rebuild is then a full one) and the error says why.
pub fn record(
    out: &Path,
    state: &State,
    detected: &Value,
    seen: &Seen<'_>,
    nodes: usize,
    edges: usize,
) -> Result<(), Changed> {
    let made = write_record(out, state, detected, seen, nodes, edges);
    if made.is_err() {
        forget(out);
    }
    made
}

fn write_record(
    out: &Path,
    state: &State,
    detected: &Value,
    seen: &Seen<'_>,
    nodes: usize,
    edges: usize,
) -> Result<(), Changed> {
    let graph = seen.graph.clone().ok_or("graph.json not read")?;
    let manifest = seen.manifest.clone().ok_or("manifest.json not written")?;
    if seen.retryable {
        return Err("a file failed to extract and may pass on a retry");
    }
    if FileStat::of(&out.join("graph.json")).as_ref() != Some(&graph) {
        return Err("graph.json changed since it was read");
    }
    if FileStat::of(&out.join("manifest.json")).as_ref() != Some(&manifest) {
        return Err("manifest.json changed since it was written");
    }
    if seen.snapshot.recent() {
        return Err("a file changed just before the scan");
    }
    if !seen.snapshot.unchanged() {
        return Err("a file changed during the rebuild");
    }
    let record = Record {
        schema: SCHEMA,
        build: state.build.clone(),
        settings: state.settings.clone(),
        above: state.above.clone(),
        scan: scan(detected),
        files: seen.snapshot.digest.clone(),
        graph,
        manifest,
        nodes,
        edges,
    };
    let text = serde_json::to_string(&record).map_err(|_| "record not written")?;
    paths::write_text_atomic(&out.join(STATE_FILE), &(text + "\n"))
        .map_err(|_| "record not written")
}

/// Remove the record: a rebuild wrote a new graph, which the next full
/// rebuild may still change, or could not vouch for what it read.
pub fn forget(out: &Path) {
    let _ = std::fs::remove_file(out.join(STATE_FILE));
}

/// Why a full rebuild does not stop early: the first check that failed.
pub type Changed = &'static str;

/// A full rebuild whose record matched until the scan.
pub struct Pending {
    record: Record,
}

/// What the early stop vouches for.
pub struct Unchanged {
    pub nodes: usize,
    pub edges: usize,
}

/// The part of the check that needs no scan: the record exists, the build,
/// settings and files above the root are the same, graph.json and
/// manifest.json are the files it recorded, and no file the manifest lists
/// has another modification time. An error takes the full rebuild.
pub fn before_scan(out: &Path, watch_root: &Path, state: &State) -> Result<Pending, Changed> {
    let text = std::fs::read_to_string(out.join(STATE_FILE)).map_err(|_| "no record")?;
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
    // A quick look before the scan: a file whose modification time moved has
    // changed (the stats after the scan decide the rest). A full rebuild then
    // reads the stored graph while it scans.
    let manifest = detect_manifest::load_manifest(&manifest_path, Some(watch_root));
    let rows = manifest
        .as_object()
        .ok_or("manifest unreadable")?
        .iter()
        .collect::<Vec<_>>();
    let moved = crate::par::map(&rows, crate::par::io_workers(), |(path, row)| {
        let recorded = if row["mtime"].is_object() {
            detect_manifest::numeric(&row["mtime"]["mtime"])
        } else {
            detect_manifest::numeric(&row["mtime"])
        };
        detect_manifest::mtime(Path::new(path)).ok() != recorded
    });
    if moved.into_iter().any(|moved| moved) {
        return Err("file times");
    }
    Ok(Pending { record })
}

impl Pending {
    /// The rest of the check, with the scan: the same paths classified the
    /// same way, and every scanned file (and hidden configuration file) with
    /// the stats the record holds. An error takes the full rebuild.
    pub fn after_scan(self, detected: &Value) -> Result<Unchanged, Changed> {
        if scan(detected) != self.record.scan {
            return Err("scanned files");
        }
        if Snapshot::take(detected).digest != self.record.files {
            return Err("file stats");
        }
        Ok(Unchanged {
            nodes: self.record.nodes,
            edges: self.record.edges,
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
            "pnpm-workspace.yaml",
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

    /// A JavaScript workspace root in a folder above the project, by
    /// `pnpm-workspace.yaml` or by a package.json with `workspaces`.
    #[test]
    fn a_workspace_above_the_root_is_seen() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let root = base.join("packages/app");
        std::fs::create_dir_all(&root).unwrap();
        assert!(!workspace_above(&root));
        std::fs::write(base.join("package.json"), r#"{"name":"monorepo"}"#).unwrap();
        assert!(!workspace_above(&root));
        std::fs::write(
            base.join("package.json"),
            r#"{"name":"monorepo","workspaces":["packages/*"]}"#,
        )
        .unwrap();
        assert!(workspace_above(&root));
        std::fs::remove_file(base.join("package.json")).unwrap();
        std::fs::write(
            base.join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/*'\n",
        )
        .unwrap();
        assert!(workspace_above(&root));
        // The project's own workspace file is scanned, not above it.
        std::fs::remove_file(base.join("pnpm-workspace.yaml")).unwrap();
        std::fs::write(root.join("pnpm-workspace.yaml"), "packages:\n  - 'lib/*'\n").unwrap();
        assert!(!workspace_above(&root));
    }

    fn state() -> State {
        State {
            build: "build".into(),
            settings: "settings".into(),
            above: "above".into(),
        }
    }

    /// A rebuild records only what still holds when it records: a file that
    /// changed after the scan (while the rebuild extracted), a graph.json or
    /// manifest.json another command wrote after the rebuild read or wrote
    /// it, or a file that failed to extract in a way a retry may pass keeps
    /// it from recording, and removes an older record.
    #[test]
    fn a_rebuild_records_only_what_still_holds() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let out = root.join(".graphicle");
        std::fs::create_dir_all(&out).unwrap();
        let source = root.join("a.py");
        let (graph, manifest) = (out.join("graph.json"), out.join("manifest.json"));
        std::fs::write(&source, "x = 1\n").unwrap();
        std::fs::write(&graph, "{\"nodes\":[],\"links\":[]}\n").unwrap();
        std::fs::write(&manifest, "{}\n").unwrap();
        let detected = json!({"files": {"code": [source.to_string_lossy()]}});
        let state = state();
        let recorded = || out.join(STATE_FILE).is_file();
        let run = |snapshot: &Snapshot, graph_seen, manifest_seen, retryable| {
            let seen = Seen {
                snapshot,
                graph: graph_seen,
                manifest: manifest_seen,
                retryable,
            };
            let made = record(&out, &state, &detected, &seen, 1, 0);
            assert_eq!(made.is_ok(), recorded());
            made
        };
        let snapshot = Snapshot::with_granularity(&detected, 0);
        let both = || (FileStat::of(&graph), FileStat::of(&manifest));
        let (g, m) = both();
        assert_eq!(run(&snapshot, g, m, false), Ok(()));
        // A file that changed after the scan.
        std::fs::write(&source, "x = 22\n").unwrap();
        let (g, m) = both();
        assert_eq!(
            run(&snapshot, g, m, false),
            Err("a file changed during the rebuild")
        );
        let snapshot = Snapshot::with_granularity(&detected, 0);
        let (g, m) = both();
        assert_eq!(run(&snapshot, g, m, false), Ok(()));
        // graph.json written by another command after the rebuild read it.
        let graph_seen = FileStat::of(&graph);
        std::fs::write(&graph, "{\"nodes\":[{\"id\":\"x\"}],\"links\":[]}\n").unwrap();
        assert_eq!(
            run(&snapshot, graph_seen, FileStat::of(&manifest), false),
            Err("graph.json changed since it was read")
        );
        // manifest.json written by another command after the rebuild wrote it.
        let manifest_seen = FileStat::of(&manifest);
        std::fs::write(&manifest, "{\"other\": 1}\n").unwrap();
        assert_eq!(
            run(&snapshot, FileStat::of(&graph), manifest_seen, false),
            Err("manifest.json changed since it was written")
        );
        // A file that failed to extract in a way a retry may pass.
        let (g, m) = both();
        assert_eq!(
            run(&snapshot, g, m, true),
            Err("a file failed to extract and may pass on a retry")
        );
        let (g, m) = both();
        assert_eq!(run(&snapshot, g, m, false), Ok(()));
        // Without a graph.json read, nothing is vouched for.
        assert_eq!(
            run(&snapshot, None, FileStat::of(&manifest), false),
            Err("graph.json not read")
        );
    }

    /// A file changed shortly before the snapshot (within the granularity)
    /// may change again with the same size and times, so its stats prove
    /// nothing yet and the rebuild records nothing.
    #[test]
    fn a_file_changed_just_before_the_scan_is_not_recorded() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let out = root.join(".graphicle");
        std::fs::create_dir_all(&out).unwrap();
        let source = root.join("a.py");
        std::fs::write(&source, "x = 1\n").unwrap();
        std::fs::write(out.join("graph.json"), "{\"nodes\":[],\"links\":[]}\n").unwrap();
        std::fs::write(out.join("manifest.json"), "{}\n").unwrap();
        let detected = json!({"files": {"code": [source.to_string_lossy()]}});
        let state = state();
        let run = |granularity_ns: u64| {
            let snapshot = Snapshot::with_granularity(&detected, granularity_ns);
            let seen = Seen {
                snapshot: &snapshot,
                graph: FileStat::of(&out.join("graph.json")),
                manifest: FileStat::of(&out.join("manifest.json")),
                retryable: false,
            };
            record(&out, &state, &detected, &seen, 1, 0)
        };
        let minute = 60_000_000_000;
        assert_eq!(run(minute), Err("a file changed just before the scan"));
        assert!(!out.join(STATE_FILE).exists());
        // An old modification time does not help: the status-change time is
        // the time of the last change.
        let file = std::fs::File::options().append(true).open(&source).unwrap();
        let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        file.set_modified(hour_ago).unwrap();
        #[cfg(unix)]
        assert_eq!(run(minute), Err("a file changed just before the scan"));
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(run(20_000_000), Ok(()));
        assert!(out.join(STATE_FILE).exists());
    }

    /// Before the scan, a file whose modification time moved since the
    /// manifest stamped it is a change; after the scan, so is any other
    /// change of its stats, with the modification time kept.
    #[test]
    fn the_check_sees_moved_times_and_kept_times() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let out = root.join(".graphicle");
        std::fs::create_dir_all(&out).unwrap();
        let source = root.join("a.py");
        std::fs::write(&source, "x = 1\n").unwrap();
        std::fs::write(out.join("graph.json"), "{\"nodes\":[],\"links\":[]}\n").unwrap();
        let spelled = source.to_string_lossy().into_owned();
        let detected = json!({"files": {"code": [spelled.clone()]}});
        let stamp = |files: &serde_json::Map<String, Value>| {
            detect_manifest::save_manifest(
                files,
                &out.join("manifest.json"),
                "ast",
                &detect_manifest::ManifestOptions {
                    root: Some(root.clone()),
                    ..Default::default()
                },
            )
            .unwrap();
        };
        stamp(detected["files"].as_object().unwrap());
        let state = state();
        let snapshot = Snapshot::with_granularity(&detected, 0);
        let seen = Seen {
            snapshot: &snapshot,
            graph: FileStat::of(&out.join("graph.json")),
            manifest: FileStat::of(&out.join("manifest.json")),
            retryable: false,
        };
        assert_eq!(record(&out, &state, &detected, &seen, 0, 0), Ok(()));
        let check = || {
            before_scan(&out, &root, &state)
                .and_then(|pending| pending.after_scan(&detected))
                .map(|_| ())
        };
        assert_eq!(check(), Ok(()));
        // The same content and size, another modification time.
        let file = std::fs::File::options().append(true).open(&source).unwrap();
        let modified = std::fs::metadata(&source).unwrap().modified().unwrap();
        file.set_modified(modified - std::time::Duration::from_secs(60))
            .unwrap();
        assert_eq!(check(), Err("file times"));
        // Other content of the same size, the old modification time back.
        // A file's status-change time moves with the kernel's clock tick, so
        // the rewrite waits past one.
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(&source, "x = 2\n").unwrap();
        let file = std::fs::File::options().append(true).open(&source).unwrap();
        file.set_modified(modified).unwrap();
        #[cfg(unix)]
        assert_eq!(check(), Err("file stats"));
    }
}
