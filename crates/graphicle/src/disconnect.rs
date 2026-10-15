//! `graphicle disconnect` and `graphicle uninstall`:
//! undo what `setup` and `connect` added outside the data folder, found
//! from the data folder's record of changes ([`crate::agent_files`]) and,
//! for changes made before that record, where Graphicle writes them.
use crate::agent_detect::{self, Environment};
use crate::agent_files::{self, Change};
use crate::platform::CanonicalExt;
use crate::setup::{Ui, native, shown, unix};
use crate::{
    Error, Result, agent_instructions, agent_setup, codex_config, git_hooks, local, store::Store,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// `graphicle disconnect`'s options: one client, else every client of this
/// folder's project; `all` undoes every change recorded or found.
#[derive(Clone, Debug, Default)]
pub struct Disconnect {
    pub client: Option<String>,
    pub all: bool,
    pub json: bool,
}

/// A change to undo: a recorded one, or one found where Graphicle writes.
#[derive(Clone, Debug)]
struct Target {
    change: Change,
    recorded: bool,
}

/// Run `graphicle disconnect` and print what it undid.
pub fn disconnect(store: Option<&Store>, data: &Path, options: &Disconnect) -> Result<()> {
    let ui = Ui::new(options.json, true);
    let report = disconnect_report(store, data, options, &Environment::current())?;
    ui.print(&report, render_disconnect)
}

/// Undo what Graphicle added: its client entries,
/// capture hooks, instruction sections and Git-hook blocks, found from the
/// data folder's record of changes and, for changes made before it, where
/// Graphicle writes them. Graphify entries a switch removed are put back.
/// Only this data folder's entries and hooks are touched: another data
/// folder's are left, and named in the report (`other_stores`).
pub fn disconnect_report(
    store: Option<&Store>,
    data: &Path,
    options: &Disconnect,
    env: &Environment,
) -> Result<Value> {
    let client = match options.client.as_deref() {
        Some(word) => Some(agent_detect::parse(word).ok_or_else(|| {
            Error::bad(format!(
                "Unknown client {word}: choose claude-code, codex, cursor, vscode or claude (Claude Desktop)"
            ))
        })?),
        None => None,
    };
    let cwd = std::env::current_dir()?;
    // This folder's project: its registered folder and id.
    let (folder, project) = match store.and_then(|store| {
        let session = local::session(store, false).ok()?;
        local::find(store, &session).ok().flatten()
    }) {
        Some(found) if found.source == "folder" => (
            found
                .folder
                .clone()
                .unwrap_or_else(|| cwd.canonical().unwrap_or(cwd.clone())),
            Some(found.id),
        ),
        _ => (cwd.canonical().unwrap_or(cwd.clone()), None),
    };
    // A record that cannot be read stops here: it holds which files
    // Graphicle created, and nothing may overwrite it.
    let recorded = agent_files::load(data)?.changes;
    let wanted =
        |change: &Change| client.is_none_or(|client| change.client.as_deref() == Some(client));
    let mine = |change: &Change| {
        options.all
            || change.folder.as_deref() == Some(folder.as_path())
            || (project.is_some() && change.project == project)
            // The backups line of the repository this folder is in.
            || (change.kind == "exclude"
                && change
                    .folder
                    .as_deref()
                    .is_some_and(|root| folder.starts_with(root)))
    };
    let mut targets: Vec<Target> = recorded
        .iter()
        .filter(|change| wanted(change) && mine(change))
        .filter(|change| change.kind != "hooks" || change.folder.is_some() || options.all)
        .filter(|change| change.kind != "exclude" || client.is_none())
        .map(|change| Target {
            change: change.clone(),
            recorded: true,
        })
        .collect();
    // Codex's capture hooks serve every project: they go with the last
    // Codex connection.
    if !options.all && client.is_none_or(|client| client == "codex") {
        let remaining = recorded.iter().any(|change| {
            change.kind == "codex" && !targets.iter().any(|target| target.change == *change)
        });
        if !remaining {
            for change in recorded
                .iter()
                .filter(|change| change.kind == "hooks" && change.folder.is_none())
            {
                targets.push(Target {
                    change: change.clone(),
                    recorded: true,
                });
            }
        }
    }
    // Where Graphicle writes, for changes made before the record.
    let mut folders = vec![folder.clone()];
    if options.all {
        for change in &recorded {
            if let Some(other) = &change.folder
                && !folders.contains(other)
            {
                folders.push(other.clone());
            }
        }
    }
    let mut found = Vec::new();
    for folder in &folders {
        found.extend(scan_folder(folder, client));
    }
    found.extend(scan_user(
        env,
        client,
        if options.all {
            None
        } else {
            project.as_deref()
        },
        data,
    ));
    for change in found {
        if !targets
            .iter()
            .any(|target| same_target(&target.change, &change))
        {
            targets.push(Target {
                change,
                recorded: false,
            });
        }
    }
    // Undo, one file at a time; the backups line of a repository last,
    // once its backups are gone.
    let (excludes, targets): (Vec<Target>, Vec<Target>) = targets
        .into_iter()
        .partition(|target| target.change.kind == "exclude");
    let mut by_file: BTreeMap<PathBuf, Vec<Target>> = BTreeMap::new();
    for target in targets {
        by_file
            .entry(target.change.path.clone())
            .or_default()
            .push(target);
    }
    let mut files = Vec::new();
    let mut done: Vec<Change> = Vec::new();
    let mut undo = |path: &Path, targets: &[Target]| {
        let outcome = match undo_file(path, targets, options.all, data) {
            Ok(outcome) => outcome,
            Err(error) => Some(json!({"file":path,"state":"failed","error":error.message})),
        };
        if outcome
            .as_ref()
            .is_none_or(|outcome| outcome["state"] != "failed")
        {
            done.extend(
                targets
                    .iter()
                    .filter(|target| target.recorded)
                    .map(|target| target.change.clone()),
            );
        }
        if let Some(outcome) = outcome {
            files.push(outcome);
        }
    };
    for (path, targets) in &by_file {
        undo(path, targets);
    }
    for exclude in excludes {
        // Kept while a backup is left in its working tree.
        let root = exclude.change.folder.clone().unwrap_or_default();
        if root.as_os_str().is_empty() || agent_files::backups_under(&root, 50_000).is_empty() {
            undo(&exclude.change.path.clone(), std::slice::from_ref(&exclude));
        }
    }
    agent_files::update(data, |left| {
        left.changes.retain(|change| !done.contains(change));
    })?;
    let kept: Vec<Value> = files
        .iter()
        .filter_map(|file| file["kept_backup"].as_str().map(|backup| json!(backup)))
        .collect();
    let mut report = json!({"disconnected":true,"scope":if options.all { "all" } else { "folder" },
        "folder":folder,"client":client,"files":files,"kept_backups":kept,
        "next":if files.is_empty() { "Nothing of Graphicle's was found to remove" } else { "Restart or reload your clients" }});
    if options.all {
        let others = other_stores(env, client, data);
        if !others.is_empty() {
            report["other_stores"] = json!(others);
        }
    }
    Ok(report)
}

/// Whether two changes name the same thing.
fn same_target(a: &Change, b: &Change) -> bool {
    a.path == b.path && a.kind == b.kind && a.entry == b.entry
}

/// Graphicle's changes in a folder's own files, found by entry names and
/// markers.
fn scan_folder(folder: &Path, client: Option<&str>) -> Vec<Change> {
    let mut found = Vec::new();
    let wanted = |name: &str| client.is_none_or(|client| client == name);
    let entry = |path: PathBuf, client: &str, group: &str| Change {
        path,
        kind: "mcp".into(),
        client: Some(client.into()),
        folder: Some(folder.to_path_buf()),
        entry: Some("graphicle".into()),
        group: Some(group.into()),
        ..Change::default()
    };
    if wanted("claude-code") {
        found.push(entry(folder.join(".mcp.json"), "claude-code", "mcpServers"));
        found.push(Change {
            path: folder.join(".claude/settings.local.json"),
            kind: "hooks".into(),
            client: Some("claude-code".into()),
            folder: Some(folder.to_path_buf()),
            ..Change::default()
        });
    }
    if wanted("vscode") {
        found.push(entry(folder.join(".vscode/mcp.json"), "vscode", "servers"));
    }
    if client.is_none() {
        for name in ["AGENTS.md", "CLAUDE.md"] {
            found.push(Change {
                path: folder.join(name),
                kind: "instructions".into(),
                folder: Some(folder.to_path_buf()),
                ..Change::default()
            });
        }
        if let Some(repo) = git_hooks::repository(folder)
            && let Ok(hooks) = repo.hooks
        {
            for name in git_hooks::HOOKS {
                found.push(Change {
                    path: hooks.join(name),
                    kind: "git-hook".into(),
                    folder: Some(folder.to_path_buf()),
                    ..Change::default()
                });
            }
        }
        if let Some((root, exclude)) = agent_files::work_tree(folder)
            && std::fs::read_to_string(&exclude)
                .is_ok_and(|text| text.contains(agent_files::EXCLUDE_START))
        {
            found.push(Change {
                path: exclude,
                kind: "exclude".into(),
                folder: Some(root),
                ..Change::default()
            });
        }
    }
    found
        .into_iter()
        .filter(|change| change.path.is_file())
        .collect()
}

/// Graphicle's changes in the user's client files: this data folder's
/// entries (naming `project`, or every one), and Codex's capture hooks
/// when every entry goes.
fn scan_user(
    env: &Environment,
    client: Option<&str>,
    project: Option<&str>,
    data: &Path,
) -> Vec<Change> {
    let mut found = Vec::new();
    let wanted = |name: &str| client.is_none_or(|client| client == name);
    for (name, file) in [
        ("cursor", env.cursor_config()),
        ("claude", env.claude_desktop_config()),
    ] {
        let (true, Some(file)) = (wanted(name), file) else {
            continue;
        };
        let Some(document) = agent_detect::json_file(&file) else {
            continue;
        };
        for entry in agent_setup::graphicle_entries(&document, "mcpServers", project, data) {
            found.push(Change {
                path: file.clone(),
                kind: "mcp".into(),
                client: Some(name.into()),
                project: project.map(str::to_string),
                entry: Some(entry),
                group: Some("mcpServers".into()),
                ..Change::default()
            });
        }
    }
    if wanted("codex") {
        if let Some(file) = env.codex_config()
            && let Ok(text) = std::fs::read_to_string(&file)
        {
            for table in codex_config::tables(&text) {
                let entry = table.entry();
                let ours = agent_files::entry_serves(&entry, data, project)
                    && (project.is_none() || agent_files::entry_project(&entry).is_some());
                if ours {
                    found.push(Change {
                        path: file.clone(),
                        kind: "codex".into(),
                        client: Some("codex".into()),
                        project: project.map(str::to_string),
                        entry: Some(table.name),
                        ..Change::default()
                    });
                }
            }
        }
        if project.is_none()
            && let Some(hooks) = env.codex_dir().map(|folder| folder.join("hooks.json"))
            && hooks.is_file()
        {
            found.push(Change {
                path: hooks,
                kind: "hooks".into(),
                client: Some("codex".into()),
                ..Change::default()
            });
        }
    }
    found
}

/// The Graphicle entries of other data folders in the user's client files,
/// which `disconnect --all` leaves: `{file, entry, data_dir}`.
fn other_stores(env: &Environment, client: Option<&str>, data: &Path) -> Vec<Value> {
    let wanted = |name: &str| client.is_none_or(|client| client == name);
    let mut others = Vec::new();
    for (name, file) in [
        ("cursor", env.cursor_config()),
        ("claude", env.claude_desktop_config()),
    ] {
        let (true, Some(file)) = (wanted(name), file) else {
            continue;
        };
        let Some(document) = agent_detect::json_file(&file) else {
            continue;
        };
        for (entry, store) in agent_setup::other_stores(&document, "mcpServers", data) {
            others.push(json!({"file":file,"entry":entry,"data_dir":store}));
        }
    }
    if wanted("codex")
        && let Some(file) = env.codex_config()
        && let Ok(text) = std::fs::read_to_string(&file)
    {
        for table in codex_config::tables(&text) {
            let entry = table.entry();
            if agent_files::is_graphicle_entry(&entry)
                && !agent_files::entry_uses_store(&entry, data)
            {
                let store = agent_files::store_of(table.args.iter().flatten().map(String::as_str));
                others.push(json!({"file":file,"entry":table.name,"data_dir":store}));
            }
        }
    }
    others
}

/// Whether a backup's bytes are the same as a file's new content.
type SameAsBackup = Box<dyn Fn(&[u8]) -> bool>;

/// Undo the changes `targets` name in `path`: remove Graphicle's part (only
/// what serves the data folder `data`), put back what a switch replaced,
/// then restore the backup when the file is the same as it, delete a file
/// that is left empty when Graphicle created it or there is no backup to
/// put back, or write what is left. `None` when nothing of Graphicle's was
/// there.
fn undo_file(path: &Path, targets: &[Target], all: bool, data: &Path) -> Result<Option<Value>> {
    if !path.is_file() || path.is_symlink() {
        return Ok(None);
    }
    let created = targets.iter().any(|target| target.change.created);
    let kind = targets[0].change.kind.as_str();
    let mut removed = Vec::new();
    let (bytes, same, empty): (Vec<u8>, SameAsBackup, bool) = match kind {
        "mcp" | "hooks" | "removed-mcp" => {
            let raw = std::fs::read_to_string(path).unwrap_or_default();
            let Some(mut document) = crate::json_edit::parse(&raw).filter(Value::is_object) else {
                // A recorded change in a file that is no longer a JSON
                // object stays recorded, for the next disconnect.
                if targets.iter().any(|target| target.recorded) {
                    return Err(Error::bad(format!(
                        "{} is not valid JSON now ({}); remove Graphicle's entries from it by hand",
                        path.display(),
                        crate::json_edit::error_place(&raw)
                    )));
                }
                return Ok(None);
            };
            // The file's text, edited in place while every step can be.
            let mut text = Some(raw.clone());
            // Graphicle's own first, then what a switch removed.
            let mut ordered: Vec<&Target> = targets.iter().collect();
            ordered.sort_by_key(|target| target.change.kind == "removed-mcp");
            for target in ordered {
                let change = &target.change;
                let group = change.group.as_deref().unwrap_or("mcpServers");
                let entry = change.entry.as_deref().unwrap_or_default();
                let done = match change.kind.as_str() {
                    "mcp" => {
                        let gone = agent_setup::remove_entry(
                            &mut document,
                            group,
                            entry,
                            data,
                            change.project.as_deref(),
                        );
                        if gone {
                            text =
                                text.and_then(|text| crate::json_edit::remove(&text, group, entry));
                        }
                        gone.then(|| format!("removed the {entry} entry"))
                    }
                    "hooks" => {
                        let client = change.client.as_deref().unwrap_or_default();
                        // Capture hooks, and Claude Code's guard hooks
                        // (setup, `connect --graph`).
                        let capture = agent_setup::holds_capture_hooks(&document, client, data);
                        let guard = agent_setup::holds_guard_hooks(&document, client, data);
                        let gone = agent_setup::remove_hooks(&mut document, client, data);
                        if gone {
                            text = None;
                        }
                        gone.then(|| {
                            match (capture, guard) {
                                (true, true) => "removed the capture and graph guard hooks",
                                (false, true) => "removed the graph guard hooks",
                                _ => "removed the capture hooks",
                            }
                            .to_string()
                        })
                    }
                    "removed-mcp" => match change.replaced.as_ref().map(|r| r["value"].clone()) {
                        Some(value) if document[group].get(entry).is_none() => {
                            if !document[group].is_object() {
                                document[group] = json!({});
                            }
                            document[group][entry] = value.clone();
                            text = text.and_then(|text| {
                                crate::json_edit::set(&text, group, entry, &value)
                            });
                            Some(format!("put back Graphify's {entry} entry"))
                        }
                        _ => None,
                    },
                    _ => None,
                };
                removed.extend(done);
            }
            let empty = document.as_object().is_some_and(|object| object.is_empty());
            let bytes = match text {
                Some(text) if crate::json_edit::parse(&text).as_ref() == Some(&document) => text,
                _ => {
                    if crate::json_edit::has_comments(&raw) && !removed.is_empty() {
                        return Err(Error::bad(format!(
                            "{} has comments that a rewrite would drop; remove Graphicle's entries from it by hand",
                            path.display()
                        )));
                    }
                    crate::json_edit::render(&document, Some(&raw))
                }
            }
            .into_bytes();
            // The same as the backup, apart from the tables and lists that
            // taking Graphicle's part out emptied and removed (a file that
            // held only `"servers": {}` before `connect`), and with the same
            // comments (a comment written or changed while connected is kept,
            // so the edited text is written): the backup's bytes are the file
            // as it was.
            let expected = pruned(&document);
            let comments: Vec<String> = crate::json_edit::comments(&raw)
                .into_iter()
                .map(str::to_string)
                .collect();
            (
                bytes,
                Box::new(move |backup: &[u8]| {
                    std::str::from_utf8(backup).is_ok_and(|backup| {
                        crate::json_edit::parse(backup)
                            .is_some_and(|saved| pruned(&saved) == expected)
                            && crate::json_edit::comments(backup) == comments
                    })
                }),
                empty,
            )
        }
        "instructions" | "git-hook" | "codex" | "removed-codex" | "exclude" => {
            let Ok(raw) = std::fs::read_to_string(path) else {
                return Ok(None);
            };
            let (mut text, crlf) = unix(&raw);
            let mut ordered: Vec<&Target> = targets.iter().collect();
            ordered.sort_by_key(|target| target.change.kind == "removed-codex");
            for target in ordered {
                let change = &target.change;
                match change.kind.as_str() {
                    "instructions" => {
                        let replaced = change
                            .replaced
                            .as_ref()
                            .and_then(|replaced| replaced["text"].as_str());
                        if let Some(new) = agent_instructions::remove(&text, replaced) {
                            text = new;
                            removed.push(if replaced.is_some() {
                                "replaced the Graphicle section with Graphify's".to_string()
                            } else {
                                "removed the Graphicle section".to_string()
                            });
                        }
                    }
                    "git-hook" => {
                        // This folder's block; every block for --all.
                        let only = if all { None } else { change.folder.as_deref() };
                        if let Some(new) = git_hooks::remove(&text, only) {
                            text = new;
                            removed.push("removed the freshness block".into());
                        }
                    }
                    "codex" => {
                        let entry = change.entry.as_deref().unwrap_or_default();
                        let ours = codex_config::tables(&text).into_iter().any(|table| {
                            table.name == entry
                                && agent_files::entry_serves(
                                    &table.entry(),
                                    data,
                                    change.project.as_deref(),
                                )
                        });
                        if ours && let Some((new, _, _)) = codex_config::remove(&text, entry) {
                            text = new;
                            removed.push(format!("removed the {entry} server"));
                        }
                    }
                    "removed-codex" => {
                        let entry = change.entry.as_deref().unwrap_or_default();
                        let there = codex_config::tables(&text)
                            .iter()
                            .any(|table| table.name == entry);
                        if let (false, Some(replaced)) = (there, change.replaced.as_ref())
                            && let Some(lines) = replaced["text"].as_str()
                        {
                            let at = replaced["line"].as_u64().unwrap_or(u64::MAX) as usize;
                            text = codex_config::reinsert(&text, lines, at);
                            removed.push(format!("put back Graphify's {entry} server"));
                        }
                    }
                    "exclude" => {
                        if let Some(new) = agent_files::remove_exclude(&text) {
                            text = new;
                            removed.push("removed the line that kept backups out of Git".into());
                        }
                    }
                    _ => {}
                }
            }
            let empty = if kind == "git-hook" {
                git_hooks::empty(&text)
            } else {
                text.trim().is_empty()
            };
            let expected = text.clone();
            let bytes = native(text, crlf).into_bytes();
            (
                bytes,
                Box::new(move |backup: &[u8]| {
                    std::str::from_utf8(backup)
                        .is_ok_and(|backup| unix(backup).0.trim_end() == expected.trim_end())
                }),
                empty,
            )
        }
        _ => return Ok(None),
    };
    if removed.is_empty() {
        return Ok(None);
    }
    let backup = agent_files::backup_path(path);
    let backed_up = std::fs::symlink_metadata(&backup).is_ok_and(|meta| meta.is_file());
    let state = if backed_up && std::fs::read(&backup).is_ok_and(|saved| same(&saved)) {
        agent_files::restore(path, &backup)?;
        "restored"
    } else if empty && (created || !backed_up) {
        // Left empty (`{}`, blank, or a hook's `#!` line alone) and nothing
        // to put back: Graphicle made it, or the record of that was lost.
        std::fs::remove_file(path)?;
        if backed_up {
            std::fs::remove_file(&backup)?;
        }
        // An empty client folder Graphicle made in the project goes too.
        if let Some(parent) = path.parent()
            && parent
                .file_name()
                .is_some_and(|name| name == ".vscode" || name == ".claude")
        {
            let _ = std::fs::remove_dir(parent);
        }
        "deleted"
    } else {
        agent_files::write(path, &bytes, 0o600)?;
        "edited"
    };
    let mut outcome = json!({"file":path,"state":state,"removed":removed});
    if state == "edited" && backed_up {
        outcome["kept_backup"] = json!(backup);
    }
    Ok(Some(outcome))
}

/// `value` without the object members that hold an empty object or an empty
/// list once their own empty members are gone.
fn pruned(value: &Value) -> Value {
    match value {
        Value::Object(members) => Value::Object(
            members
                .iter()
                .map(|(key, member)| (key.clone(), pruned(member)))
                .filter(|(_, member)| {
                    !(member.as_object().is_some_and(serde_json::Map::is_empty)
                        || member.as_array().is_some_and(Vec::is_empty))
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(pruned).collect()),
        other => other.clone(),
    }
}

/// Phrases joined for a sentence: `a`, `a and b`, `a, b and c`.
fn listed(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => (*one).to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// The text of a disconnect report.
pub fn render_disconnect(report: &Value) -> String {
    let env = Environment::current();
    let folder = report["folder"].as_str().map(PathBuf::from);
    let mut out = String::new();
    let files = report["files"].as_array().cloned().unwrap_or_default();
    if files.is_empty() {
        out.push_str("Nothing of Graphicle's was found to remove.\n");
    }
    for file in &files {
        let path = shown(
            Path::new(file["file"].as_str().unwrap_or_default()),
            folder.as_deref(),
            &env,
        );
        let what: Vec<&str> = file["removed"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let line = match file["state"].as_str() {
            Some("restored") => format!(
                "{}; the file is back as it was (from its backup)",
                listed(&what)
            ),
            Some("deleted") => format!("{}; the file, now empty, was deleted", listed(&what)),
            Some("failed") => format!("failed: {}", file["error"].as_str().unwrap_or_default()),
            _ => listed(&what),
        };
        out.push_str(&format!("{path}: {line}\n"));
    }
    for backup in report["kept_backups"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "Kept {}: your copy from before Graphicle; delete it when you no longer need it.\n",
            shown(
                Path::new(backup.as_str().unwrap_or_default()),
                folder.as_deref(),
                &env
            )
        ));
    }
    for other in report["other_stores"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "Left {} in {}: it serves another data folder ({}).\n",
            other["entry"].as_str().unwrap_or_default(),
            shown(
                Path::new(other["file"].as_str().unwrap_or_default()),
                None,
                &env
            ),
            shown(
                Path::new(other["data_dir"].as_str().unwrap_or_default()),
                None,
                &env
            )
        ));
    }
    if !files.is_empty() {
        out.push_str("Restart or reload your clients.\n");
    }
    out
}

/// `graphicle uninstall [--purge]`: disconnect everywhere, then say how to
/// remove the program; `--purge` also deletes the data folder after
/// confirmation (`--yes` without a terminal).
pub fn uninstall(data: &Path, purge: bool, yes: bool, json_flag: bool) -> Result<()> {
    let ui = Ui::new(json_flag, yes);
    let env = Environment::current();
    // Confirmed before anything is undone.
    let confirmed = purge
        && (yes || ui
            .ask(&format!(
                "Delete {} with every project, memory and session in it? Type delete to confirm:",
                data.display()
            ))
            .is_some_and(|answer| answer == "delete"));
    if purge && !confirmed && !ui.interactive {
        return Err(Error::bad(
            "--purge deletes the data folder: add --yes to confirm when not in a terminal",
        ));
    }
    let store = if data.join("reviews.sqlite3").is_file() {
        Some(Store::open(data)?)
    } else {
        None
    };
    let disconnected = disconnect_report(
        store.as_ref(),
        data,
        &Disconnect {
            client: None,
            all: true,
            json: json_flag,
        },
        &env,
    )?;
    drop(store);
    let executable = std::env::current_exe()?.canonical()?;
    let install = crate::terminal::installed(&executable);
    let mut report = json!({"disconnected":disconnected,"remove_program":removal(&executable, install.method),
        "data_dir":data,"also":[
            "graphicle graph uninstall removes the code-graph engine's skills and rules, and graphicle graph hook uninstall its Git hooks, in each repository where you installed them",
            "the engine's settings folder ~/.graphicle-graph/ and each project's .graphicle/ folder"]});
    if purge {
        report["purged"] = json!(confirmed && purge_data(data, &env)?);
    }
    ui.print(&report, render_uninstall)
}

/// Delete the data folder, when it is one.
fn purge_data(data: &Path, env: &Environment) -> Result<bool> {
    if !data.exists() {
        return Ok(false);
    }
    let data = data.canonical()?;
    let home = env.home.as_deref().and_then(|home| home.canonical().ok());
    if !data.join("reviews.sqlite3").is_file()
        || data.parent().is_none()
        || home.as_deref() == Some(data.as_path())
    {
        return Err(Error::bad(format!(
            "{} does not look like a Graphicle data folder; delete it yourself",
            data.display()
        )));
    }
    std::fs::remove_dir_all(&data)?;
    Ok(true)
}

/// How to remove this program, for the way it was installed (`method`, as
/// `graphicle update` finds it: [`crate::terminal::installed`]).
fn removal(executable: &Path, method: &str) -> Value {
    let text = executable.to_string_lossy();
    if method == "homebrew" {
        return json!({"method":"homebrew","command":"brew uninstall graphicle"});
    }
    if method == "docker" {
        // The containers and their images; the data volume stays.
        return json!({"method":"docker","command":"docker compose down --rmi all"});
    }
    if method == "cargo" {
        // The package Cargo installed this program from, as Cargo's install
        // record in its home folder names it; else Graphicle Core's.
        let binary = executable
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let package = executable
            .parent()
            .and_then(Path::parent)
            .and_then(|home| std::fs::read(home.join(".crates2.json")).ok())
            .and_then(|record| cargo_package(&record, &binary))
            .unwrap_or_else(|| "graphicle-runtime".to_string());
        return json!({"method":"cargo","command":format!("cargo uninstall {package}")});
    }
    // The install script's programs, or a source build's.
    let engine =
        executable.with_file_name(format!("graphicle-graph{}", std::env::consts::EXE_SUFFIX));
    let method = if method == "source" {
        "source"
    } else {
        "files"
    };
    // Windows: a PowerShell command (single quotes, a quote doubled).
    #[cfg(windows)]
    {
        let quote = |path: &str| format!("'{}'", path.replace('\'', "''"));
        let mut files = vec![quote(&text)];
        if engine.is_file() {
            files.push(quote(&engine.to_string_lossy()));
        }
        json!({"method":method,"command":format!("Remove-Item -LiteralPath {}", files.join(", "))})
    }
    #[cfg(not(windows))]
    {
        let mut files = vec![git_hooks::quote(&text)];
        if engine.is_file() {
            files.push(git_hooks::quote(&engine.to_string_lossy()));
        }
        json!({"method":method,"command":format!("rm {}", files.join(" "))})
    }
}

/// The package of Cargo's install record (`.crates2.json`) whose programs
/// include `binary`.
fn cargo_package(record: &[u8], binary: &str) -> Option<String> {
    let record: Value = serde_json::from_slice(record).ok()?;
    record["installs"]
        .as_object()?
        .iter()
        .find_map(|(id, install)| {
            install["bins"]
                .as_array()?
                .iter()
                .any(|name| name == binary)
                .then(|| id.split(' ').next().unwrap_or(id).to_string())
        })
}

/// The text of an uninstall report.
pub fn render_uninstall(report: &Value) -> String {
    let mut out = render_disconnect(&report["disconnected"]);
    out.push_str(&format!(
        "\nTo remove the program: {}\n",
        report["remove_program"]["command"]
            .as_str()
            .unwrap_or_default()
    ));
    for also in report["also"].as_array().into_iter().flatten() {
        out.push_str(&format!("Also: {}\n", also.as_str().unwrap_or_default()));
    }
    match report.get("purged").and_then(Value::as_bool) {
        Some(true) => out.push_str(&format!(
            "Deleted the data folder {}.\n",
            report["data_dir"].as_str().unwrap_or_default()
        )),
        Some(false) => out.push_str("The data folder was kept.\n"),
        None => out.push_str(&format!(
            "Your data stays in {}; graphicle uninstall --purge deletes it.\n",
            report["data_dir"].as_str().unwrap_or_default()
        )),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn windows_removal_is_a_powershell_command() {
        let removal = removal(Path::new(r"C:\Users\o'neil\bin\graphicle.exe"), "script");
        assert_eq!(
            removal["command"],
            r"Remove-Item -LiteralPath 'C:\Users\o''neil\bin\graphicle.exe'"
        );
    }

    #[test]
    fn removal_follows_the_install() {
        // The method `graphicle update` finds for each program path.
        let home = Path::new("/home/a");
        let cargo = Path::new("/home/a/.cargo/bin");
        let remove = |exe: &str, docker: bool| {
            let exe = Path::new(exe);
            let install = crate::terminal::install_method(exe, Some(home), Some(cargo), docker);
            removal(exe, install.method)
        };
        assert_eq!(
            remove("/opt/homebrew/Cellar/graphicle/0.1.0/bin/graphicle", false)["command"],
            "brew uninstall graphicle"
        );
        assert_eq!(
            remove("/home/linuxbrew/.linuxbrew/bin/graphicle", false)["method"],
            "homebrew"
        );
        assert_eq!(
            remove("/home/a/.cargo/bin/graphicle", false)["method"],
            "cargo"
        );
        // The removal command is for the platform's shell: `rm` on macOS
        // and Linux, PowerShell's Remove-Item on Windows.
        let rm = if cfg!(windows) {
            "Remove-Item -LiteralPath "
        } else {
            "rm "
        };
        assert_eq!(
            remove("/home/a/.local/bin/graphicle", false)["command"],
            format!("{rm}'/home/a/.local/bin/graphicle'")
        );
        assert_eq!(
            remove("/usr/local/bin/graphicle", true)["command"],
            "docker compose down --rmi all"
        );
        let built = remove(
            "/home/a/src/graphicle/runtime/target/release/graphicle",
            false,
        );
        assert_eq!(built["method"], "source");
        assert!(built["command"].as_str().unwrap().starts_with(rm));
        let record = br#"{"installs":{"other 1.0.0 (registry+https://github.com/rust-lang/crates.io-index)":{"bins":["other"]},"graphicle-runtime 0.1.0 (path+file:///src/graphicle)":{"bins":["graphicle"]}}}"#;
        assert_eq!(
            cargo_package(record, "graphicle").as_deref(),
            Some("graphicle-runtime")
        );
        assert_eq!(cargo_package(record, "missing"), None);
        assert_eq!(cargo_package(b"not json", "graphicle"), None);
    }

    /// A file that held only empty tables before `connect` matches what
    /// is left once Graphicle's part and the tables it emptied are gone.
    #[test]
    fn emptied_tables_do_not_count_against_the_backup() {
        assert_eq!(pruned(&json!({"servers":{}})), json!({}));
        assert_eq!(
            pruned(&json!({"hooks":{"PreToolUse":[]},"permissions":{"allow":["Bash(ls)"]}})),
            json!({"permissions":{"allow":["Bash(ls)"]}})
        );
        assert_eq!(pruned(&json!({"a":[{}],"b":0})), json!({"a":[{}],"b":0}));
        assert_ne!(
            pruned(&json!({"servers":{"other":{"command":"other-server"}}})),
            json!({})
        );
    }
}
