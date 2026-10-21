//! `graphicle offboard`: remove one project (one client's data) from this
//! data folder, with a hash-chained receipt that lists what was removed.
//!
//! It covers, all inside Graphicle's own data folder:
//! - the project in the store, with its PR scratch copies: the certified
//!   project deletion of [`crate::deletion`] (sources, memory, graph,
//!   history and what was derived from them), whose receipt joins the
//!   workspace's receipt chain and lists the files below;
//! - its graph cache (`gf-cache/<project>`), the materialized copy of its
//!   code and graph;
//! - its copies in the backups `graphicle backup` keeps in the data folder
//!   (`backups/`): the project is deleted from each backup the same way, with
//!   a receipt in that backup's own chain, and the backup is compacted.
//!   Other projects in the backup stay.
//!
//! Nothing outside the data folder is touched: a symbolic link, a backup
//! file with another name (a hard link, which may lie outside the data
//! folder), or a path that resolves outside it, is refused (listed, never
//! followed or changed); on Windows, which gives no link count, so is every
//! backup. Backups and
//! exports written elsewhere, what Graphicle added in the project's folders
//! (`graphicle disconnect` removes it) and provider-side retention are named
//! as not covered.
//!
//! The plan comes first: [`plan`] changes nothing and ends with a hash;
//! [`apply`] takes that hash and carries out exactly that plan, or refuses
//! when anything in it changed.
use crate::platform::CanonicalExt;
use crate::store::{Principal, Store, one, rows, s};
use crate::{Error, Result, digest, local};
use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The folder of the data folder where `graphicle backup` without a path
/// writes, and where offboarding looks for backups.
pub const BACKUPS: &str = "backups";
/// The graph cache folder ([`crate::graph_projection::cache_root`]).
const CACHE: &str = "gf-cache";
/// What offboarding cannot reach.
pub const NOT_COVERED: &[&str] = &[
    "backups and exports written outside the data folder",
    "what Graphicle added in the project's folders: agent entries, hooks and their .graphicle-bak backups (graphicle disconnect removes them)",
    "the graph engine's output folder in the project's folders (.graphicle/, or the folder GRAPHICLE_OUT names)",
    "model-provider retention of requests already sent",
    "memories that memory.import_pack or memory.import copied into other projects, which belong to those projects",
];

/// Where `graphicle backup` without a path writes:
/// `backups/graphicle-<UTC time>.sqlite3` in the data folder, which is
/// created private. A `backups` that is not a folder (a symbolic link
/// included) is refused.
pub fn backup_path(store: &Store) -> Result<PathBuf> {
    let folder = store.directory.join(BACKUPS);
    match std::fs::symlink_metadata(&folder) {
        Ok(meta) if !meta.is_dir() => {
            return Err(Error::bad(format!(
                "{} is not a folder (Graphicle keeps its backups there); name the backup file instead",
                folder.display()
            )));
        }
        Ok(_) => {}
        Err(_) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                std::fs::DirBuilder::new().mode(0o700).create(&folder)?;
            }
            #[cfg(not(unix))]
            std::fs::create_dir(&folder)?;
        }
    }
    Ok(folder.join(format!(
        "graphicle-{}.sqlite3",
        chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ")
    )))
}

/// `parts` joined under the canonical data folder `root` when no step is a
/// symbolic link and the result resolves inside `root`: `Ok(None)` when it
/// does not exist, `Err(reason)` when it is refused.
fn inside(root: &Path, parts: &[&str]) -> std::result::Result<Option<PathBuf>, &'static str> {
    let mut path = root.to_path_buf();
    for part in parts {
        if part.is_empty() || *part == "." || *part == ".." || part.contains('/') {
            return Err("not a plain name");
        }
        path.push(part);
        match std::fs::symlink_metadata(&path) {
            Err(_) => return Ok(None),
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(
                    "a symbolic link: Graphicle does not follow links out of its data folder",
                );
            }
            Ok(_) => {}
        }
    }
    match path.canonical() {
        Ok(canonical) if canonical.starts_with(root) => Ok(Some(path)),
        _ => Err("outside the data folder"),
    }
}

/// Why the file at `path` may have another name, which may lie outside the
/// data folder and which changing the file would change too: it is a hard
/// link (more than one name). Windows gives no link count through the
/// standard library, so there every backup counts as possibly linked and is
/// left as it is.
fn other_name(path: &Path) -> Option<&'static str> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path)
            .is_ok_and(|meta| meta.nlink() > 1)
            .then_some("a hard link: the same file may have another name outside the data folder")
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Some(
            "Graphicle cannot tell on this system whether the file has another name (a hard link) outside the data folder",
        )
    }
}

/// Files and bytes under `folder`, links not followed.
fn tally(folder: &Path) -> (u64, u64) {
    let (mut files, mut bytes) = (0, 0);
    let mut stack = vec![folder.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                files += 1;
                bytes += meta.len();
            }
        }
    }
    (files, bytes)
}

/// SHA-256 and length of a file, read in pieces.
fn file_sha256(path: &Path) -> Result<(String, u64)> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 16];
    let mut length = 0u64;
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        length += n as u64;
    }
    Ok((format!("{:x}", hasher.finalize()), length))
}

/// Rows the store holds for `scope`, by table: every table with a
/// `repository_id` column.
fn counts(db: &Connection, scope: &[String]) -> Result<BTreeMap<String, i64>> {
    let mut counts = BTreeMap::new();
    for table in rows(
        db,
        "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name",
        [],
    )? {
        let table = s(&table, "name");
        let has = one(
            db,
            "SELECT 1 AS yes FROM pragma_table_info(?) WHERE name='repository_id'",
            [table],
        )?
        .is_some();
        if !has {
            continue;
        }
        let sql = format!(
            "SELECT count(*) FROM \"{}\" WHERE repository_id=?",
            table.replace('"', "\"\"")
        );
        let mut n = 0i64;
        for repository in scope {
            n += db.query_row(&sql, [repository], |r| r.get::<_, i64>(0))?;
        }
        if n > 0 {
            counts.insert(table.to_string(), n);
        }
    }
    Ok(counts)
}

/// The projects of `scope` (and the PR scratch copies of `project` only the
/// backup still records) that a backup holds, how many other projects it
/// holds, and whether any of them belongs to another workspace.
struct Held {
    ids: Vec<String>,
    others: i64,
    foreign: bool,
}

fn held_in(db: &Connection, project: &str, scope: &[String], workspace: &str) -> Result<Held> {
    let mut ids = scope.to_vec();
    let scratch = one(
        db,
        "SELECT 1 AS yes FROM sqlite_master WHERE type='table' AND name='repo_pr_scratch'",
        [],
    )?
    .is_some();
    if scratch {
        for row in rows(
            db,
            "SELECT repository_id FROM repo_pr_scratch WHERE project_id=? ORDER BY repository_id",
            [project],
        )? {
            let copy = s(&row, "repository_id").to_string();
            if !ids.contains(&copy) {
                ids.push(copy);
            }
        }
    }
    let (mut present, mut foreign) = (Vec::new(), false);
    for id in ids {
        if let Some(row) = one(
            db,
            "SELECT workspace_id FROM repo_projects WHERE id=?",
            [&id],
        )? {
            foreign |= s(&row, "workspace_id") != workspace;
            present.push(id);
        }
    }
    let total: i64 = db.query_row("SELECT count(*) FROM repo_projects", [], |r| r.get(0))?;
    Ok(Held {
        others: total - present.len() as i64,
        ids: present,
        foreign,
    })
}

/// [`held_in`] for a backup file, opened immutable so that reading it
/// changes nothing; `None` when it is not a readable Graphicle store.
fn held(path: &Path, project: &str, scope: &[String], workspace: &str) -> Option<Held> {
    let mut uri = String::from("file:");
    for c in path.to_str()?.chars() {
        match c {
            '%' => uri.push_str("%25"),
            '?' => uri.push_str("%3f"),
            '#' => uri.push_str("%23"),
            c => uri.push(c),
        }
    }
    uri.push_str("?immutable=1");
    let db = Connection::open_with_flags(
        &uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .ok()?;
    held_in(&db, project, scope, workspace).ok()
}

/// The backups in the data folder's `backups` folder that hold a project of
/// `scope`; links and files outside the data folder are added to `refused`,
/// files that are not readable Graphicle stores to `skipped`.
fn backups(
    root: &Path,
    project: &str,
    scope: &[String],
    workspace: &str,
    refused: &mut Vec<Value>,
    skipped: &mut Vec<Value>,
) -> Result<Vec<Value>> {
    let folder = match inside(root, &[BACKUPS]) {
        Ok(Some(folder)) => folder,
        Ok(None) => return Ok(Vec::new()),
        Err(reason) => {
            refused.push(json!({"path": BACKUPS, "reason": reason}));
            return Ok(Vec::new());
        }
    };
    let mut names = Vec::new();
    for entry in std::fs::read_dir(&folder)?.flatten() {
        match entry.file_name().into_string() {
            Ok(name) => names.push(name),
            Err(name) => refused.push(json!({
                "path": format!("{BACKUPS}/{}", name.to_string_lossy()),
                "reason": "not a UTF-8 name",
            })),
        }
    }
    names.sort();
    let mut found = Vec::new();
    for name in names {
        if ["-wal", "-shm", "-journal"]
            .iter()
            .any(|suffix| name.ends_with(suffix))
        {
            continue;
        }
        let relative = format!("{BACKUPS}/{name}");
        let path = match inside(root, &[BACKUPS, &name]) {
            Ok(Some(path)) if path.is_file() => path,
            Ok(_) => continue,
            Err(reason) => {
                refused.push(json!({"path": relative, "reason": reason}));
                continue;
            }
        };
        if let Some(reason) = other_name(&path) {
            refused.push(json!({"path": relative, "reason": reason}));
            continue;
        }
        let Some(held) = held(&path, project, scope, workspace) else {
            skipped.push(json!({"path": relative, "reason": "not a readable Graphicle store"}));
            continue;
        };
        if held.ids.is_empty() {
            continue;
        }
        if held.foreign {
            refused.push(
                json!({"path": relative, "reason": "holds the project for another workspace"}),
            );
            continue;
        }
        let (sha256, bytes) = file_sha256(&path)?;
        found.push(json!({
            "path": relative,
            "sha256": sha256,
            "bytes": bytes,
            "projects": held.ids.len(),
            "other_projects": held.others,
        }));
    }
    Ok(found)
}

/// The offboarding plan for the project `requested` (an id, a key or a
/// synced folder): what goes, what is refused and what is not covered,
/// with the hash [`apply`] needs. Nothing changes. Only the workspace's
/// owner may offboard.
pub fn plan(store: &Store, session: &local::Session, requested: &str) -> Result<Value> {
    let id = local::project(store, session, Some(requested))?;
    let root = store.directory.canonical()?;
    let (project, copies, store_rows, folders) = store.authorized_read(
        &session.workspace,
        &session.capability,
        "admin",
        |db, p| {
            let project = crate::store::repo(db, p, &id)?;
            let copies = crate::folders::scratch_projects(db, &id)?;
            let scope: Vec<String> = std::iter::once(id.clone())
                .chain(copies.iter().cloned())
                .collect();
            let store_rows = counts(db, &scope)?;
            let folders: Vec<String> = rows(
                db,
                "SELECT path FROM repo_folders WHERE repository_id=? AND path IS NOT NULL ORDER BY path",
                [&id],
            )?
            .iter()
            .map(|r| s(r, "path").to_string())
            .collect();
            Ok((project, copies, store_rows, folders))
        },
    )?;
    let scope: Vec<String> = std::iter::once(id.clone())
        .chain(copies.iter().cloned())
        .collect();
    let (mut refused, mut skipped, mut cache) = (Vec::new(), Vec::new(), Vec::new());
    let mut blocked = false;
    for repository in &scope {
        let relative = format!("{CACHE}/{repository}");
        match inside(&root, &[CACHE, repository]) {
            Ok(Some(path)) => {
                let (files, bytes) = tally(&path);
                cache.push(json!({"path": relative, "files": files, "bytes": bytes}));
            }
            Ok(None) => {}
            Err(reason) => {
                // The project deletion removes this folder: refuse it all.
                blocked = true;
                refused.push(json!({"path": relative, "reason": reason, "blocks": true}));
            }
        }
    }
    let found = backups(
        &root,
        &id,
        &scope,
        &session.workspace,
        &mut refused,
        &mut skipped,
    )?;
    let snapshot = s(&project, "snapshot").to_string();
    let material = json!({
        "project": id, "snapshot": snapshot, "pr_scratch_projects": copies,
        "store_rows": store_rows, "graph_cache": cache, "backups": found,
        "refused": refused,
    });
    let plan_hash = digest(&material);
    let next = if blocked {
        "Remove the refused links, then run graphicle offboard again".to_string()
    } else {
        format!("graphicle offboard --project {id} --apply {plan_hash}")
    };
    Ok(json!({
        "dry_run": true,
        "project": {
            "id": id, "key": project["project_key"], "name": project["name"],
            "snapshot": snapshot, "folders": folders,
        },
        "pr_scratch_projects": copies,
        "store_rows": store_rows,
        "graph_cache": cache,
        "backups": found,
        "refused": refused,
        "skipped": skipped,
        "blocked": blocked,
        "not_covered": NOT_COVERED,
        "data_dir": root.to_string_lossy(),
        "plan_hash": plan_hash,
        "before_you_apply": "Run graphicle disconnect in the project's folders first, so that its Git hooks and agents do not sync it again.",
        "next": next,
    }))
}

/// Carry out the plan [`plan`] showed with hash `expected`: delete the
/// project from each backup that holds it, then from the store (its graph
/// cache with it), and forget it as the saved default project. Refused when
/// the plan changed or a refused path blocks it. `verified` holds only when
/// every deletion checks out and no path was refused: a refused path was
/// never changed and may still hold the project.
pub fn apply(
    store: &Store,
    session: &local::Session,
    requested: &str,
    expected: &str,
) -> Result<Value> {
    let shown = plan(store, session, requested)?;
    if shown["plan_hash"].as_str() != Some(expected) {
        return Err(Error::conflict(format!(
            "The offboarding plan changed since it was shown, or {expected} is not its hash: run graphicle offboard --project {requested} again and pass the plan hash it prints"
        )));
    }
    if shown["blocked"] == true {
        return Err(Error::bad(
            "A path the offboarding would remove is a symbolic link or lies outside the data folder; Graphicle removes nothing outside its data folder. Remove the link and run graphicle offboard again",
        ));
    }
    let id = s(&shown["project"], "id").to_string();
    let root = store.directory.canonical()?;
    let scope: Vec<String> = std::iter::once(id.clone())
        .chain(
            shown["pr_scratch_projects"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_string),
        )
        .collect();
    let principal =
        store.authorized_read(&session.workspace, &session.capability, "admin", |_, p| {
            Ok(p.clone())
        })?;
    let mut cleaned = Vec::new();
    for backup in shown["backups"].as_array().into_iter().flatten() {
        cleaned.push(scrub(
            &root,
            s(backup, "path"),
            s(backup, "sha256"),
            &id,
            &scope,
            &principal,
        )?);
    }
    let offboarding = json!({
        "plan_hash": expected,
        "graph_cache": shown["graph_cache"],
        "backups": cleaned,
        "refused": shown["refused"],
        "not_covered": NOT_COVERED,
    });
    // The backups are cleaned first so that the store's receipt can list
    // them. Should the store's deletion then fail (the project changed in the
    // meantime, say), the error names the backups already cleaned, whose own
    // receipt chains record it; running offboard again finishes the store.
    let deleted = store
        .offboard_project(
            &session.workspace,
            &session.capability,
            &id,
            s(&shown["project"], "snapshot"),
            offboarding,
        )
        .map_err(|error| {
            if cleaned.is_empty() {
                return error;
            }
            let paths: Vec<&str> = cleaned.iter().map(|backup| s(backup, "path")).collect();
            Error {
                message: format!(
                    "{} The project was already removed from {} (each with a receipt in that backup's own chain), but not from the store: run graphicle offboard --project {id} again to remove it there.",
                    error.message,
                    paths.join(", ")
                ),
                ..error
            }
        })?;
    let cache_left: Vec<&str> = shown["graph_cache"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|entry| s(entry, "path"))
        .filter(|path| root.join(path).exists())
        .collect();
    if session.project.as_deref() == Some(id.as_str()) {
        local::save_session(
            store,
            &local::Session {
                project: None,
                ..session.clone()
            },
        )?;
    }
    // A refused path was never changed, so it may still hold the project.
    let refused = shown["refused"].as_array().map_or(0, Vec::len);
    let verified = deleted["receipt"]["verified_local"] == true
        && cache_left.is_empty()
        && cleaned.iter().all(|backup| backup["verified"] == true)
        && refused == 0;
    Ok(json!({
        "offboarded": true,
        "project": id,
        "verified": verified,
        "receipt": deleted["receipt"],
        "pr_scratch_projects": deleted["pr_scratch_projects"],
        "graph_cache": shown["graph_cache"],
        "graph_cache_left": cache_left,
        "backups": cleaned,
        "refused": shown["refused"],
        "not_covered": NOT_COVERED,
        "storage": deleted["storage"],
    }))
}

/// Delete the projects of `scope` (and the PR scratch copies only the
/// backup records) from the backup `relative`, as the store deletes them,
/// with receipts in the backup's own chain; then pause its pending external
/// cleanup as `graphicle backup` does, compact it and check it.
fn scrub(
    root: &Path,
    relative: &str,
    planned_sha256: &str,
    project: &str,
    scope: &[String],
    p: &Principal,
) -> Result<Value> {
    let changed = || {
        Error::conflict(format!(
            "{relative} changed since the plan was shown; run graphicle offboard again"
        ))
    };
    let name = relative
        .strip_prefix(&format!("{BACKUPS}/"))
        .ok_or_else(changed)?;
    let Ok(Some(path)) = inside(root, &[BACKUPS, name]) else {
        return Err(changed());
    };
    if other_name(&path).is_some() {
        return Err(changed());
    }
    // The bytes the plan was shown for, opened as they are: never created.
    let (before, bytes_before) = file_sha256(&path)?;
    if before != planned_sha256 {
        return Err(changed());
    }
    let mut db = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    db.busy_timeout(std::time::Duration::from_secs(10))?;
    db.pragma_update(None, "foreign_keys", true)?;
    db.pragma_update(None, "secure_delete", true)?;
    let transaction = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let held = held_in(&transaction, project, scope, &p.workspace)?;
    if held.foreign {
        return Err(changed());
    }
    // The PR scratch copies first, then the project, as `project.delete`.
    // Only the backup file changes: a graph cache would be the `gf-cache`
    // folder beside it, which the plan never covers and may be a link.
    let mut order = held.ids.clone();
    order.sort_by_key(|id| id == project);
    let mut receipts = Vec::with_capacity(order.len());
    for id in &order {
        let receipt = crate::maintenance::delete_project(
            &transaction,
            p,
            id,
            json!({"offboarding": {"backup": relative}}),
            false,
        )?;
        receipts.push(json!({
            "sequence": receipt["sequence"],
            "digest": receipt["digest"],
            "verified_local": receipt["verified_local"],
        }));
    }
    transaction.execute(
        "UPDATE am_projection_cleanup SET state='paused',lease_token=NULL,lease_until=NULL WHERE state!='complete'",
        [],
    )?;
    transaction.commit()?;
    db.execute_batch("VACUUM;")?;
    db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
    let integrity: String = db.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    let left = held_in(&db, project, scope, &p.workspace)?.ids.len();
    drop(db);
    let (after, bytes_after) = file_sha256(&path)?;
    let verified =
        integrity == "ok" && left == 0 && receipts.iter().all(|r| r["verified_local"] == true);
    Ok(json!({
        "path": relative,
        "sha256_before": before,
        "bytes_before": bytes_before,
        "sha256_after": after,
        "bytes_after": bytes_after,
        "projects_removed": order.len(),
        "other_projects_kept": held.others,
        "integrity": integrity,
        "verified": verified,
        "receipts": receipts,
    }))
}

/// The readable view of [`plan`] and [`apply`].
pub fn text(value: &Value, _: crate::terminal::Style) -> String {
    let mut out = Vec::new();
    let list = |key: &str| value[key].as_array().cloned().unwrap_or_default();
    if value["offboarded"] == true {
        let receipt = &value["receipt"];
        out.push(format!(
            "Offboarded project {}. Deletion receipt {} (digest {}){}.",
            s(value, "project"),
            receipt["sequence"],
            s(receipt, "digest"),
            if value["verified"] == true {
                ", verified"
            } else {
                ", NOT fully verified: see the receipt with --json"
            }
        ));
        let refused = list("refused").len();
        if refused > 0 {
            out.push(format!(
                "  {refused} refused {} not changed and may still hold the project",
                if refused == 1 {
                    "path was"
                } else {
                    "paths were"
                }
            ));
        }
        for backup in list("backups") {
            out.push(format!(
                "  removed from backup {} ({} other projects kept)",
                s(&backup, "path"),
                backup["other_projects_kept"]
            ));
        }
        for cache in list("graph_cache") {
            out.push(format!("  removed graph cache {}", s(&cache, "path")));
        }
    } else {
        let project = &value["project"];
        out.push(format!(
            "Offboarding plan for project {} ({}). Nothing has been changed.",
            s(project, "name"),
            s(project, "id")
        ));
        let tables: Vec<String> = value["store_rows"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(table, n)| format!("{table} {n}"))
            .collect();
        out.push(format!("  store rows: {}", tables.join(", ")));
        let copies = list("pr_scratch_projects").len();
        if copies > 0 {
            out.push(format!("  PR scratch copies: {copies}"));
        }
        for cache in list("graph_cache") {
            out.push(format!(
                "  graph cache {} ({} files, {} bytes)",
                s(&cache, "path"),
                cache["files"],
                cache["bytes"]
            ));
        }
        for backup in list("backups") {
            out.push(format!(
                "  backup {}: the project is removed from it; its {} other projects stay",
                s(&backup, "path"),
                backup["other_projects"]
            ));
        }
    }
    for refused in list("refused") {
        out.push(format!(
            "  refused, never touched: {} ({})",
            s(&refused, "path"),
            s(&refused, "reason")
        ));
    }
    out.push("  not covered:".to_string());
    for item in NOT_COVERED {
        out.push(format!("    - {item}"));
    }
    if value["offboarded"] != true {
        let folders: Vec<&str> = value["project"]["folders"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        if !folders.is_empty() {
            out.push(format!(
                "Before you apply, run graphicle disconnect in {}, so that its Git hooks and agents do not sync it again.",
                folders.join(", ")
            ));
        }
        out.push(format!("Next: {}", s(value, "next")));
    }
    out.join("\n")
}
