//! Work state: an agent's own to-do lists and tracked working state for a
//! project, shown at the start of later sessions until closed.
//!
//! Adapted from claude-mem 47a5151 (Apache-2.0; see NOTICE.md). Entries are
//! append-only and a list is replayed in order, the latest value of each key
//! winning, so a write names only what changed. An entry with a `task` field
//! updates that item; any other entry updates the list's own state. A status
//! of "done" or "dropped" (any case) closes an item, or the whole list; null
//! clears a key from what is shown.
//!
//! Graphicle's additions: string values pass the capture policy (`<private>`
//! spans dropped, credentials redacted) before they are stored; control
//! characters are shown as spaces, so no value can add lines of its own;
//! writing needs `propose` permission and reading `read`; entries belong to
//! the repository, are deleted with it and are covered by deletion's residue
//! scans; a repository holds at most [`MAX_ENTRIES`] entries. The tools are
//! published only when enabled (`graphicle mcp --work-state`).
use crate::capture::Applied;
use crate::store::{Store, repo, s};
use crate::{Error, Result};
use rusqlite::{Connection, params};
use serde_json::{Map, Value, json};
use std::collections::HashMap;

/// Characters of open work in the session-start section and a write's reply.
pub const SECTION_CHARS: usize = 3_000;
/// One write's fields as JSON: a few short fields, not documents.
pub const MAX_FIELDS_JSON_CHARS: usize = 2_000;
pub const MAX_LIST_CHARS: usize = 200;
pub const MAX_KEY_CHARS: usize = 100;
/// Entries one repository may hold (at most about 100 MB of fields).
pub const MAX_ENTRIES: i64 = 50_000;

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS am_work_state (
 id INTEGER PRIMARY KEY,
 repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
 list_name TEXT NOT NULL, fields TEXT NOT NULL,
 created_at TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS am_work_state_list ON am_work_state(repository_id, list_name, id);";

pub fn migrate(db: &Connection) -> Result<()> {
    db.execute_batch(SCHEMA)?;
    Ok(())
}

/// One stored write.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub id: i64,
    pub list: String,
    pub fields: Map<String, Value>,
    pub at_ms: i64,
}

/// One list replayed: its own state and its items in first-seen order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Folded {
    pub state: Map<String, Value>,
    pub state_at_ms: Option<i64>,
    pub tasks: Vec<Task>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Task {
    pub name: String,
    pub fields: Map<String, Value>,
    pub at_ms: i64,
}

/// Replay one list's entries (oldest first): the latest value of each key wins.
pub fn fold<'a>(entries: impl IntoIterator<Item = &'a Entry>) -> Folded {
    let mut folded = Folded::default();
    let mut index: HashMap<String, usize> = HashMap::new();
    for entry in entries {
        let Some(name) = task_name(&entry.fields) else {
            merge(&mut folded.state, &entry.fields);
            folded.state_at_ms = Some(entry.at_ms);
            continue;
        };
        if let Some(&at) = index.get(&name) {
            let task = &mut folded.tasks[at];
            merge(&mut task.fields, &entry.fields);
            task.at_ms = entry.at_ms;
        } else {
            index.insert(name.clone(), folded.tasks.len());
            folded.tasks.push(Task {
                name,
                fields: entry.fields.clone(),
                at_ms: entry.at_ms,
            });
        }
    }
    folded
}

/// Keys already present keep their place (the map preserves insertion order).
fn merge(into: &mut Map<String, Value>, from: &Map<String, Value>) {
    for (key, value) in from {
        into.insert(key.clone(), value.clone());
    }
}

/// The item an entry updates: its `task` value, unless missing, null or "".
fn task_name(fields: &Map<String, Value>) -> Option<String> {
    match fields.get("task")? {
        Value::Null => None,
        Value::String(name) if name.is_empty() => None,
        value => Some(text(value)),
    }
}

fn text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn closed(status: Option<&Value>) -> bool {
    status.is_some_and(|status| {
        !status.is_null()
            && matches!(
                text(status).trim().to_lowercase().as_str(),
                "done" | "dropped"
            )
    })
}

/// Control characters, line breaks included, are shown as spaces.
fn one_line(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// `key=value` pairs; null values are left out (null clears a key).
fn details(fields: &Map<String, Value>, omitted: &[&str]) -> String {
    fields
        .iter()
        .filter(|(key, value)| !omitted.contains(&key.as_str()) && !value.is_null())
        .map(|(key, value)| format!("{}={}", one_line(key), one_line(&text(value))))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Elapsed time in words: minutes under an hour, hours under two days, then days.
pub fn describe_duration(ms: i64) -> String {
    let minutes = (ms as f64 / 60_000.0).round().max(1.0) as i64;
    if minutes < 60 {
        return format!("{minutes} minute{}", if minutes == 1 { "" } else { "s" });
    }
    let hours = (minutes as f64 / 60.0).round() as i64;
    if hours < 48 {
        return format!("about {hours} hour{}", if hours == 1 { "" } else { "s" });
    }
    format!("about {} days", (hours as f64 / 24.0).round() as i64)
}

fn ago(at_ms: i64, now_ms: i64) -> String {
    format!("updated {} ago", describe_duration(now_ms - at_ms))
}

/// Lines for one list. A closed list hides its state line but still shows
/// any item left open in it; `include_closed` shows everything.
pub fn render_list(list: &str, folded: &Folded, now_ms: i64, include_closed: bool) -> Vec<String> {
    let tasks: Vec<String> = folded
        .tasks
        .iter()
        .filter(|task| include_closed || !closed(task.fields.get("status")))
        .map(|task| {
            let status = match task.fields.get("status") {
                None | Some(Value::Null) => String::new(),
                Some(status) => format!("[{}] ", one_line(&text(status))),
            };
            let details = details(&task.fields, &["task", "status"]);
            let details = if details.is_empty() {
                details
            } else {
                format!(" ({details})")
            };
            format!(
                "  - {status}{}{details}, {}",
                one_line(&task.name),
                ago(task.at_ms, now_ms)
            )
        })
        .collect();
    let state = details(&folded.state, &["task"]);
    let show_state = !state.is_empty() && (include_closed || !closed(folded.state.get("status")));
    if !show_state && tasks.is_empty() {
        return Vec::new();
    }
    let header = if show_state {
        let at = folded.state_at_ms.unwrap_or(now_ms);
        format!("- {}: {state}, {}", one_line(list), ago(at, now_ms))
    } else {
        format!("- {}", one_line(list))
    };
    std::iter::once(header).chain(tasks).collect()
}

/// Lines for every list in `entries` (oldest first), the most recently
/// written list first.
pub fn render(entries: &[Entry], now_ms: i64, include_closed: bool) -> Vec<String> {
    let mut lists: Vec<(&str, Vec<&Entry>)> = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();
    for entry in entries {
        let at = *index.entry(&entry.list).or_insert_with(|| {
            lists.push((&entry.list, Vec::new()));
            lists.len() - 1
        });
        lists[at].1.push(entry);
    }
    let last = |list: &(&str, Vec<&Entry>)| list.1.last().map_or(0, |entry| entry.id);
    lists.sort_by_key(|list| std::cmp::Reverse(last(list)));
    lists
        .iter()
        .flat_map(|(list, group)| {
            render_list(list, &fold(group.iter().copied()), now_ms, include_closed)
        })
        .collect()
}

/// `heading`, then the whole lines that fit in `limit` characters, then how
/// many were left out.
pub fn fit(heading: &str, lines: &[String], limit: usize) -> String {
    let mut out = heading.to_string();
    let mut used = out.chars().count();
    for (at, line) in lines.iter().enumerate() {
        let remaining = lines.len() - at;
        let overflow = format!(
            "\n- {remaining} more line{} not shown; work_state_read lists them",
            if remaining == 1 { "" } else { "s" }
        );
        let candidate = used + 1 + line.chars().count();
        let room = if at + 1 < lines.len() {
            overflow.chars().count()
        } else {
            0
        };
        if candidate + room > limit {
            return out + &overflow;
        }
        out.push('\n');
        out.push_str(line);
        used = candidate;
    }
    out
}

/// A list name: trimmed, 1–[`MAX_LIST_CHARS`] characters on one line.
/// Absent or blank is `None` (every list, for a read).
fn list_name(args: &Value) -> Result<Option<String>> {
    let Some(value) = args.get("list").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let name = value
        .as_str()
        .ok_or_else(|| Error::bad("list must be a string"))?
        .trim();
    if name.is_empty() {
        return Ok(None);
    }
    if name.chars().count() > MAX_LIST_CHARS || name.chars().any(char::is_control) {
        return Err(Error::bad(format!(
            "list must be at most {MAX_LIST_CHARS} characters on one line"
        )));
    }
    Ok(Some(name.to_string()))
}

fn json_chars(fields: &Map<String, Value>) -> Result<usize> {
    Ok(serde_json::to_string(fields)?.chars().count())
}

/// A write's fields, checked and passed through the capture policy.
fn write_fields(args: &Value) -> Result<(Map<String, Value>, Applied)> {
    let fields = args
        .get("fields")
        .and_then(Value::as_object)
        .filter(|fields| !fields.is_empty())
        .ok_or_else(|| Error::bad("fields must be an object that sets at least one key"))?;
    let too_long = || {
        Error::bad(format!(
            "fields must be at most {MAX_FIELDS_JSON_CHARS} characters as JSON"
        ))
    };
    if json_chars(fields)? > MAX_FIELDS_JSON_CHARS {
        return Err(too_long());
    }
    let mut applied = Applied::default();
    let mut out = Map::new();
    for (key, value) in fields {
        if key.is_empty()
            || key.chars().count() > MAX_KEY_CHARS
            || key.chars().any(char::is_control)
        {
            return Err(Error::bad(format!(
                "Each field name must be 1–{MAX_KEY_CHARS} characters on one line"
            )));
        }
        let value = match value {
            Value::String(text) => Value::String(crate::capture::clean(text, &mut applied)),
            Value::Number(_) | Value::Bool(_) | Value::Null => value.clone(),
            _ => {
                return Err(Error::bad(
                    "Field values must be strings, numbers, booleans or null",
                ));
            }
        };
        out.insert(key.clone(), value);
    }
    // Redaction markers may lengthen a value; what is stored keeps the bound.
    if json_chars(&out)? > MAX_FIELDS_JSON_CHARS {
        return Err(too_long());
    }
    Ok((out, applied))
}

/// The repository's entries (one list's, when named), oldest first.
fn entries(db: &Connection, repository: &str, list: Option<&str>) -> Result<Vec<Entry>> {
    let mut statement = db.prepare(
        "SELECT id,list_name,fields,created_at FROM am_work_state
         WHERE repository_id=?1 AND (?2 IS NULL OR list_name=?2) ORDER BY id",
    )?;
    let rows = statement.query_map(params![repository, list], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, list, fields, at) = row?;
        let at_ms = chrono::DateTime::parse_from_rfc3339(&at)
            .map_err(|_| Error::bad("A work-state entry has an unreadable time"))?
            .timestamp_millis();
        out.push(Entry {
            id,
            list,
            fields: serde_json::from_str(&fields)?,
            at_ms,
        });
    }
    Ok(out)
}

/// The session-start section: what is still open in every list, within
/// [`SECTION_CHARS`]; `None` when nothing is open.
pub(crate) fn start_section(
    db: &Connection,
    repository: &str,
    now_ms: i64,
) -> Result<Option<String>> {
    let lines = render(&entries(db, repository, None)?, now_ms, false);
    Ok((!lines.is_empty()).then(|| {
        fit(
            "Open work an agent recorded (work_state_write; not reviewed). Status \"done\" or \"dropped\" closes an item or list:",
            &lines,
            SECTION_CHARS,
        )
    }))
}

/// How an agent uses the tools, for the MCP server's instructions.
pub const INSTRUCTIONS: &str = "Track to-do lists and multi-step work with work_state_write: one list per to-do list or tracked topic. An item is fields {\"task\": name, \"status\": \"todo\", \"doing\", \"done\" or \"dropped\", plus any details}; fields without a task set the list's own state. Each write names only what changed: the latest value of each key wins and null clears it. Whatever is still open is shown at the start of later sessions; work_state_read with include_closed shows closed items too.";

impl Store {
    /// `work_state.write` (propose): append one entry to a list of the
    /// repository. The reply shows what is still open in that list.
    pub fn work_state_write(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        let list = list_name(args)?.ok_or_else(|| Error::bad("list must name a list"))?;
        let (fields, applied) = write_fields(args)?;
        let encoded = serde_json::to_string(&fields)?;
        self.authorized(workspace, token, "propose", |db, p| {
            let repository = repo(db, p, crate::string(args, "repository_id", 100)?)?;
            let repository = s(&repository, "id");
            let held: i64 = db.query_row(
                "SELECT count(*) FROM am_work_state WHERE repository_id=?",
                [repository],
                |row| row.get(0),
            )?;
            if held >= MAX_ENTRIES {
                return Err(Error::bad(format!(
                    "This project's work state is full ({MAX_ENTRIES} entries)"
                )));
            }
            let at = chrono::Utc::now();
            let now_ms = at.timestamp_millis();
            db.execute(
                "INSERT INTO am_work_state(repository_id,list_name,fields,created_at) VALUES(?,?,?,?)",
                params![
                    repository,
                    list,
                    encoded,
                    at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
                ],
            )?;
            let entry = db.last_insert_rowid();
            let lines = render(&entries(db, repository, Some(&list))?, now_ms, false);
            let saved = format!("Saved to \"{}\".", one_line(&list));
            let text = if lines.is_empty() {
                format!("{saved} Nothing in it is open now.")
            } else {
                fit(&format!("{saved} Open in it:"), &lines, SECTION_CHARS)
            };
            Ok(json!({"saved":true,"entry_id":entry,"list":list,"text":text,
                "policy":{"private_spans_dropped":applied.private_spans,"credentials_redacted":applied.credentials}}))
        })
    }

    /// `work_state.read` (read): the repository's lists, most recently
    /// written first; open items only unless `include_closed`, and one list
    /// when `list` names it.
    pub fn work_state_read(&self, workspace: &str, token: &str, args: &Value) -> Result<Value> {
        let list = list_name(args)?;
        let include_closed = match args.get("include_closed") {
            None | Some(Value::Null) => false,
            Some(value) => value
                .as_bool()
                .ok_or_else(|| Error::bad("include_closed must be a boolean"))?,
        };
        self.authorized_read(workspace, token, "read", |db, p| {
            let repository = repo(db, p, crate::string(args, "repository_id", 100)?)?;
            let now_ms = chrono::Utc::now().timestamp_millis();
            let lines = render(
                &entries(db, s(&repository, "id"), list.as_deref())?,
                now_ms,
                include_closed,
            );
            let scope = list
                .as_deref()
                .map(|list| format!(" in \"{}\"", one_line(list)))
                .unwrap_or_default();
            let text = if !lines.is_empty() {
                lines.join("\n")
            } else if include_closed {
                format!("Nothing recorded{scope}.")
            } else {
                format!("Nothing open{scope}; include_closed shows closed items too.")
            };
            Ok(json!({"list":list,"include_closed":include_closed,"lines":lines.len(),"text":text}))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE: i64 = 60_000;

    fn entry(id: i64, list: &str, fields: Value, minutes_ago: i64) -> Entry {
        Entry {
            id,
            list: list.into(),
            fields: fields.as_object().unwrap().clone(),
            at_ms: 1_000 * MINUTE - minutes_ago * MINUTE,
        }
    }
    const NOW: i64 = 1_000 * MINUTE;

    #[test]
    fn the_latest_value_of_each_key_wins_and_items_keep_their_first_place() {
        let entries = [
            entry(
                1,
                "release",
                json!({"task":"tag","status":"todo","owner":"ana"}),
                30,
            ),
            entry(2, "release", json!({"phase":"freeze","blocked":"ci"}), 20),
            entry(3, "release", json!({"task":"notes","status":"doing"}), 15),
            entry(4, "release", json!({"task":"tag","status":"doing"}), 10),
            entry(5, "release", json!({"task":"","blocked":null}), 5),
        ];
        let folded = fold(&entries);
        assert_eq!(
            folded
                .tasks
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            ["tag", "notes"]
        );
        assert_eq!(folded.tasks[0].fields["status"], "doing");
        assert_eq!(folded.tasks[0].fields["owner"], "ana");
        assert_eq!(folded.tasks[0].at_ms, NOW - 10 * MINUTE);
        assert_eq!(folded.state_at_ms, Some(NOW - 5 * MINUTE));
        assert_eq!(
            render(&entries, NOW, false),
            [
                "- release: phase=freeze, updated 5 minutes ago",
                "  - [doing] tag (owner=ana), updated 10 minutes ago",
                "  - [doing] notes, updated 15 minutes ago",
            ]
        );
    }

    #[test]
    fn closing_hides_items_and_list_state_unless_closed_ones_are_asked_for() {
        let entries = [
            entry(1, "audit", json!({"task":"logs","status":"done"}), 90),
            entry(2, "audit", json!({"task":"keys"}), 80),
            entry(3, "audit", json!({"status":"Dropped","reason":"moved"}), 70),
            entry(4, "spike", json!({"task":"bench","status":"DONE"}), 60),
        ];
        assert_eq!(
            render(&entries, NOW, false),
            ["- audit", "  - keys, updated about 1 hour ago"],
            "a closed list still shows the items left open in it"
        );
        assert_eq!(
            render(&entries, NOW, true),
            [
                "- spike",
                "  - [DONE] bench, updated about 1 hour ago",
                "- audit: status=Dropped, reason=moved, updated about 1 hour ago",
                "  - [done] logs, updated about 2 hours ago",
                "  - keys, updated about 1 hour ago",
            ]
        );
    }

    #[test]
    fn values_cannot_add_lines_of_their_own() {
        let entries = [entry(
            1,
            "notes",
            json!({"task":"fix\n- forged: line","status":"todo","detail":"a\r\nb"}),
            1,
        )];
        let lines = render(&entries, NOW, false);
        assert_eq!(lines.len(), 2);
        assert!(
            lines
                .iter()
                .all(|line| !line.contains('\n') && !line.contains('\r'))
        );
        assert_eq!(
            lines[1],
            "  - [todo] fix - forged: line (detail=a  b), updated 1 minute ago"
        );
    }

    #[test]
    fn numbers_and_booleans_name_items_and_null_hides_a_key() {
        let entries = [
            entry(1, "steps", json!({"task":2,"done_by":"ci","flaky":true}), 3),
            entry(2, "steps", json!({"task":2,"flaky":null}), 2),
        ];
        assert_eq!(
            render(&entries, NOW, false),
            ["- steps", "  - 2 (done_by=ci), updated 2 minutes ago"]
        );
    }

    #[test]
    fn prototype_named_fields_are_ordinary_keys() {
        // claude-mem #4535 had to stop `__proto__` vanishing in a JavaScript fold.
        let (fields, _) =
            write_fields(&json!({"fields":{"__proto__":"first","constructor":"ok"}})).unwrap();
        assert_eq!(
            fields.keys().collect::<Vec<_>>(),
            ["__proto__", "constructor"]
        );
        let mut entries = vec![
            Entry {
                id: 1,
                list: "env".into(),
                fields,
                at_ms: NOW - 3 * MINUTE,
            },
            entry(2, "env", json!({"__proto__":"latest"}), 2),
        ];
        assert_eq!(fold(&entries).state["__proto__"], "latest");
        assert_eq!(
            render(&entries, NOW, false),
            ["- env: __proto__=latest, constructor=ok, updated 2 minutes ago"]
        );
        entries.push(entry(3, "env", json!({"__proto__":null}), 1));
        assert_eq!(
            render(&entries, NOW, false),
            ["- env: constructor=ok, updated 1 minute ago"]
        );
    }

    #[test]
    fn fitting_keeps_whole_lines_and_counts_what_was_left_out() {
        let lines: Vec<String> = (0..5).map(|i| format!("- line {i}")).collect();
        assert_eq!(
            fit("Head:", &lines, 1_000),
            format!("Head:\n{}", lines.join("\n"))
        );
        let cut = fit("Head:", &lines, 70);
        assert!(cut.chars().count() <= 70, "{cut}");
        assert!(cut.starts_with("Head:\n- line 0\n"), "{cut}");
        assert!(
            cut.ends_with("more lines not shown; work_state_read lists them"),
            "{cut}"
        );
        let shown = cut.lines().filter(|l| l.starts_with("- line")).count();
        assert!(
            cut.contains(&format!("- {} more lines", 5 - shown)),
            "{cut}"
        );
    }

    #[test]
    fn durations_read_as_minutes_then_hours_then_days() {
        for (ms, words) in [
            (0, "1 minute"),
            (-5 * MINUTE, "1 minute"),
            (89_000, "1 minute"),
            (90_000, "2 minutes"),
            (59 * MINUTE, "59 minutes"),
            (60 * MINUTE, "about 1 hour"),
            (47 * 60 * MINUTE, "about 47 hours"),
            (48 * 60 * MINUTE, "about 2 days"),
            (10 * 24 * 60 * MINUTE, "about 10 days"),
        ] {
            assert_eq!(describe_duration(ms), words, "{ms}");
        }
    }

    #[test]
    fn writes_are_checked_and_cleaned_before_they_are_stored() {
        assert!(write_fields(&json!({"fields":{}})).is_err());
        assert!(write_fields(&json!({"fields":{"":"x"}})).is_err());
        assert!(write_fields(&json!({"fields":{"a\nb":"x"}})).is_err());
        assert!(write_fields(&json!({"fields":{"k":["x"]}})).is_err());
        assert!(write_fields(&json!({"fields":{"k":"x".repeat(MAX_FIELDS_JSON_CHARS)}})).is_err());
        let (fields, applied) = write_fields(&json!({"fields":{
            "task":"rotate","note":"key ghp_abcdefghijklmnopqrstuvwxyz0123456789 <private>home address</private>done"}}))
        .unwrap();
        assert!(
            !fields["note"].as_str().unwrap().contains("ghp_abcdefghij"),
            "{fields:?}"
        );
        assert!(
            !fields["note"].as_str().unwrap().contains("home address"),
            "{fields:?}"
        );
        assert_eq!((applied.credentials, applied.private_spans), (1, 1));
        assert_eq!(
            list_name(&json!({"list":"  release  "}))
                .unwrap()
                .as_deref(),
            Some("release")
        );
        assert_eq!(list_name(&json!({"list":"   "})).unwrap(), None);
        assert!(list_name(&json!({"list":"a\nb"})).is_err());
        assert!(list_name(&json!({"list":"x".repeat(MAX_LIST_CHARS + 1)})).is_err());
    }
}
