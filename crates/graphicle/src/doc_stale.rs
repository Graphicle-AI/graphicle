//! Stale doc mentions: a passage of a document that names a code definition
//! is flagged when that definition changed after the passage last changed.
//!
//! The `link` job ([`crate::doc_links`]) resolves each mention of a document
//! to the one definition it names. When it writes a document's rows it also
//! keeps, per passage and mention, a baseline (`gf_doc_mention_baselines`):
//! the definition the mention resolved to, with its `stable_id` and
//! `revision` (the hash of its own source lines), as it was when the job
//! first read that passage in its current form. A passage is a run of
//! non-blank lines; it is known by the SHA-256 of its text, and a mention in
//! it by its text and its rank among the passage's mentions with that text.
//! A later run that finds the passage unchanged keeps its baselines, so:
//! - a mention that resolves to a definition whose revision is no longer the
//!   baseline's is `changed`;
//! - a mention that no longer resolves (its definition was renamed or
//!   deleted) is `missing`;
//! - a mention that resolves to several definitions is never flagged.
//!
//! A passage whose text changed gets new baselines, the definitions as they
//! are then, which clears its flags; edits elsewhere in the document leave
//! them. The judgment is written on the link row (`gf_doc_links.stale`, the
//! baseline beside it in `gf_doc_links.baseline`) whenever the job writes
//! the document's rows, which it does when the document or the file of a
//! definition it names changes. Rows of an extension's producer are never
//! judged.
//!
//! Baselines start when a passage is first linked: a passage that was
//! already out of date when Graphicle first read it is not flagged.
//!
//! Readers: `graph.doc_links` (each mention's `stale`, and `stale: true` to
//! list a project's flagged mentions), recall ([`annotate`]: one `Note:`
//! line on an excerpt that holds a flagged mention) and `graphicle wiki
//! check`.
use crate::db::{Connection, params};
use crate::{
    Result,
    store::{rows, s},
};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

/// A mention whose definition changed after its passage did.
pub const CHANGED: &str = "changed";
/// A mention whose definition is gone (renamed or deleted).
pub const MISSING: &str = "missing";
/// Names one excerpt's note lists; the rest are counted.
const NOTE_NAMES: usize = 3;

/// The baselines (additive), one per passage and mention, with the project
/// of the document.
pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS gf_doc_mention_baselines (
    repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
    document_id TEXT NOT NULL, passage TEXT NOT NULL, mention TEXT NOT NULL,
    target TEXT NOT NULL, recorded_at TEXT NOT NULL,
    PRIMARY KEY(repository_id,document_id,passage,mention));
";

/// The passages of `content`: maximal runs of non-blank lines, as byte
/// ranges without their last line break.
pub fn passages(content: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    let mut end = 0;
    let mut at = 0;
    for line in content.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        if body.trim().is_empty() {
            if let Some(first) = start.take() {
                out.push((first, end));
            }
        } else {
            if start.is_none() {
                start = Some(at);
            }
            end = at + body.len();
        }
        at += line.len();
    }
    if let Some(first) = start {
        out.push((first, end));
    }
    out
}

/// The passage of `content` that holds byte `at`, if any.
pub fn passage_at(spans: &[(usize, usize)], at: usize) -> Option<(usize, usize)> {
    let found = spans.partition_point(|&(_, end)| end <= at);
    spans.get(found).copied().filter(|&(start, _)| start <= at)
}

/// Each mention's passage key, `<sha256 of its passage>:<rank>`: the rank
/// counts the earlier mentions of the same passage with the same text.
/// `mentions` are `(byte_start, byte_end, text)`; a mention outside every
/// passage (it cannot be, being text) is keyed by its own bytes.
pub fn keys(content: &str, mentions: &[(usize, usize, &str)]) -> Vec<String> {
    let spans = passages(content);
    let mut order: Vec<usize> = (0..mentions.len()).collect();
    order.sort_by_key(|&i| (mentions[i].0, mentions[i].1));
    let mut digests: HashMap<(usize, usize), String> = HashMap::new();
    let mut ranks: HashMap<((usize, usize), &str), usize> = HashMap::new();
    let mut out = vec![String::new(); mentions.len()];
    for i in order {
        let (a, b, text) = mentions[i];
        let span = passage_at(&spans, a).unwrap_or((a, b.max(a)));
        let digest = digests
            .entry(span)
            .or_insert_with(|| {
                let bytes = content.get(span.0..span.1).unwrap_or("").as_bytes();
                graphicle_memory::text::sha256(bytes)
            })
            .clone();
        let rank = ranks.entry((span, text)).or_insert(0);
        out[i] = format!("{digest}:{rank}");
        *rank += 1;
    }
    out
}

/// One document's baselines while the `link` job writes its rows.
#[derive(Default)]
pub struct Baselines {
    stored: HashMap<(String, String), Value>,
    fresh: Vec<(String, String)>,
    seen: HashSet<(String, String)>,
}

impl Baselines {
    /// The baselines of `document` of `project`.
    pub fn load(db: &Connection, project: &str, document: &str) -> Result<Self> {
        let mut stored = HashMap::new();
        for row in rows(
            db,
            "SELECT passage,mention,target,recorded_at FROM gf_doc_mention_baselines \
             WHERE repository_id=? AND document_id=?",
            params![project, document],
        )? {
            let mut target: Value = serde_json::from_str(s(&row, "target")).unwrap_or(Value::Null);
            if !target.is_object() {
                continue;
            }
            target["recorded_at"] = row["recorded_at"].clone();
            stored.insert(
                (
                    s(&row, "passage").to_string(),
                    s(&row, "mention").to_string(),
                ),
                target,
            );
        }
        Ok(Self {
            stored,
            ..Self::default()
        })
    }

    /// How one row reads against its baseline: its flag (`changed` or
    /// `missing`) and the baseline, or `None`. `state` is the row's
    /// (`linked`, `ambiguous` or `unresolved`) and `target` the definition a
    /// linked row resolved to (`stable_id`, `revision`, ...). A linked row
    /// whose passage has no baseline for it gets one, recorded at `stamp`:
    /// the definition as it is now.
    pub fn judge(
        &mut self,
        passage: &str,
        mention: &str,
        state: &str,
        target: Option<&Value>,
        stamp: &str,
    ) -> Option<(&'static str, Value)> {
        let key = (passage.to_string(), mention.to_string());
        self.seen.insert(key.clone());
        match (state, self.stored.get(&key)) {
            ("linked", Some(baseline)) => {
                let then = baseline["revision"].as_str();
                let now = target.and_then(|t| t["revision"].as_str());
                match (then, now) {
                    (Some(then), Some(now)) if then != now => Some((CHANGED, baseline.clone())),
                    _ => None,
                }
            }
            ("linked", None) => {
                if let Some(target) = target.filter(|t| t["stable_id"].is_string()) {
                    let mut kept = target.clone();
                    kept["recorded_at"] = json!(stamp);
                    self.stored.insert(key.clone(), kept);
                    self.fresh.push(key);
                }
                None
            }
            ("unresolved", Some(baseline)) => Some((MISSING, baseline.clone())),
            _ => None,
        }
    }

    /// Write the new baselines and remove those of passages and mentions
    /// the document no longer has.
    pub fn save(self, db: &Connection, project: &str, document: &str) -> Result<()> {
        let mut insert = db.prepare_cached(
            "INSERT INTO gf_doc_mention_baselines(repository_id,document_id,passage,mention,target,recorded_at) \
             VALUES(?,?,?,?,?,?) ON CONFLICT(repository_id,document_id,passage,mention) \
             DO UPDATE SET target=excluded.target,recorded_at=excluded.recorded_at",
        )?;
        for key in &self.fresh {
            let mut target = self.stored[key].clone();
            let stamp = target
                .as_object_mut()
                .and_then(|fields| fields.remove("recorded_at"))
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_else(crate::now);
            insert.execute(params![
                project,
                document,
                key.0,
                key.1,
                target.to_string(),
                stamp
            ])?;
        }
        let mut delete = db.prepare_cached(
            "DELETE FROM gf_doc_mention_baselines WHERE repository_id=? AND document_id=? \
             AND passage=? AND mention=?",
        )?;
        for key in self.stored.keys().filter(|key| !self.seen.contains(*key)) {
            delete.execute(params![project, document, key.0, key.1])?;
        }
        Ok(())
    }
}

/// Remove the baselines of a document that is gone or no longer read.
pub fn forget(db: &Connection, project: &str, document: &str) -> Result<usize> {
    Ok(db.execute(
        "DELETE FROM gf_doc_mention_baselines WHERE repository_id=? AND document_id=?",
        params![project, document],
    )?)
}

/// The flag of a stored link row as a reader sees it: `{state, since, was}`
/// (the baseline's definition and when it was recorded), or `None`.
pub fn view(link: &Value) -> Option<Value> {
    let state = link["stale"].as_str().filter(|v| !v.is_empty())?;
    let baseline: Value = link["baseline"]
        .as_str()
        .and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or(Value::Null);
    Some(json!({"state":state,"since":baseline["recorded_at"],
        "was":{"repository_id":baseline["repository_id"],"stable_id":baseline["stable_id"],
            "revision":baseline["revision"],"qualified_name":baseline["qualified_name"],
            "relpath":baseline["relpath"],"line":baseline["line"]}}))
}

/// One flagged mention inside an excerpt, as [`annotate`] says it.
pub struct Flagged {
    pub mention: String,
    pub state: String,
    /// The definition's file, when it is in the excerpt's own project.
    pub relpath: Option<String>,
}

/// The note of an excerpt holding `flagged` mentions: one line.
pub fn note(flagged: &[Flagged]) -> Option<String> {
    let name = |item: &Flagged| match (item.state.as_str(), &item.relpath) {
        (MISSING, _) => format!("`{}` (no longer defined)", item.mention),
        (_, Some(path)) => format!("`{}` ({path})", item.mention),
        _ => format!("`{}`", item.mention),
    };
    match flagged {
        [] => None,
        [one] if one.state == MISSING => Some(format!(
            "may be stale: this passage names `{}`, which no longer exists (renamed or deleted)",
            one.mention
        )),
        [one] => Some(format!(
            "may be stale: this passage names {}, which changed after the passage was written",
            name(one)
        )),
        many => {
            let shown: Vec<String> = many.iter().take(NOTE_NAMES).map(name).collect();
            let more = many.len().saturating_sub(NOTE_NAMES);
            let rest = if more > 0 {
                format!(" and {more} more")
            } else {
                String::new()
            };
            Some(format!(
                "may be stale: this passage names code that changed after it was written: {}{rest}",
                shown.join(", ")
            ))
        }
    }
}

/// Recall: each repository or connector document excerpt among `hits` that
/// holds a flagged mention (its link rows computed for the excerpt's bytes)
/// gets one `age_marks` line, which the packet prints as a `Note:`. Returns
/// the packet's `stale_mentions` report.
pub fn annotate(links: &crate::doc_links::Lookup<'_>, hits: &mut [Value]) -> Result<Value> {
    let mut marked = 0u64;
    let mut mentions = 0u64;
    for hit in hits.iter_mut() {
        let source = &hit["source"];
        let (Some(a), Some(b)) = (source["byte_start"].as_u64(), source["byte_end"].as_u64())
        else {
            continue;
        };
        if !matches!(
            source["source_kind"].as_str(),
            Some("repository_file" | "connector_source")
        ) {
            continue;
        }
        let flagged =
            links.stale_in(s(source, "id"), s(source, "sha256"), a as usize, b as usize)?;
        let Some(line) = note(&flagged) else {
            continue;
        };
        mentions += flagged.len() as u64;
        marked += 1;
        let marks = &mut hit["source"]["age_marks"];
        if !marks.is_array() {
            *marks = json!([]);
        }
        if let Some(list) = marks.as_array_mut() {
            list.insert(0, json!(line));
        }
    }
    Ok(json!({"status":"checked","excerpts_marked":marked,"mentions":mentions}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passages_are_runs_of_non_blank_lines() {
        let text = "# Title\n\nOne `a` line\nand `b`.\n  \r\nLast `a`\r\n";
        let found = passages(text);
        let shown: Vec<&str> = found.iter().map(|&(x, y)| &text[x..y]).collect();
        assert_eq!(shown, ["# Title", "One `a` line\nand `b`.", "Last `a`"]);
        assert_eq!(
            passage_at(&found, text.find("`b`").unwrap()),
            Some(found[1])
        );
        assert_eq!(passage_at(&found, text.find("\n\n").unwrap() + 1), None);
        assert!(passages("").is_empty());
        assert_eq!(passages("no break"), vec![(0, 8)]);
    }

    #[test]
    fn keys_follow_the_passage_text_and_rank_repeated_names() {
        let text = "Intro `f`.\n\nUse `f`, then `g`, then `f` again.\n";
        // The name inside each marked span.
        let at = |span: &str, name: &'static str| {
            let a = text.find(span).unwrap() + 1;
            (a, a + name.len(), name)
        };
        let mentions = vec![
            at("`f`,", "f"),
            at("`g`", "g"),
            at("`f` again", "f"),
            at("`f`.", "f"),
        ];
        let found = keys(text, &mentions);
        let second =
            graphicle_memory::text::sha256("Use `f`, then `g`, then `f` again.".as_bytes());
        assert_eq!(found[0], format!("{second}:0"));
        assert_eq!(found[1], format!("{second}:0"));
        assert_eq!(found[2], format!("{second}:1"));
        let first = graphicle_memory::text::sha256("Intro `f`.".as_bytes());
        assert_eq!(found[3], format!("{first}:0"));
        // An edit elsewhere keeps the key; an edit to the passage changes it.
        let elsewhere = format!("New intro.\n\n{}", &text[text.find("Use").unwrap()..]);
        let a = elsewhere.find("`f`").unwrap() + 1;
        assert_eq!(keys(&elsewhere, &[(a, a + 1, "f")])[0], found[0]);
        let edited = text.replace("then `g`", "and `g`");
        let a = edited.find("Use `f`").unwrap() + 5;
        assert_ne!(keys(&edited, &[(a, a + 1, "f")])[0], found[0]);
    }

    fn target(revision: &str) -> Value {
        json!({"repository_id":"p","stable_id":"s1","revision":revision,"qualified_name":"wire.parse",
            "relpath":"src/wire.py","line":6})
    }

    #[test]
    fn a_baseline_flags_a_changed_or_missing_definition_and_never_an_ambiguous_one() {
        let mut first = Baselines::default();
        assert!(
            first
                .judge("k:0", "parse", "linked", Some(&target("r1")), "t1")
                .is_none()
        );
        assert_eq!(first.fresh.len(), 1);
        let stored = first.stored.clone();
        let again = |state: &str, revision: Option<&str>| {
            let mut later = Baselines {
                stored: stored.clone(),
                ..Baselines::default()
            };
            let found = revision.map(target);
            later
                .judge("k:0", "parse", state, found.as_ref(), "t2")
                .map(|(flag, was)| (flag, was["revision"].clone(), was["recorded_at"].clone()))
        };
        assert_eq!(again("linked", Some("r1")), None);
        assert_eq!(
            again("linked", Some("r2")),
            Some((CHANGED, json!("r1"), json!("t1")))
        );
        assert_eq!(
            again("unresolved", None),
            Some((MISSING, json!("r1"), json!("t1")))
        );
        assert_eq!(again("ambiguous", None), None);
        // No baseline yet: an unresolved or ambiguous row is not judged and
        // records nothing.
        let mut empty = Baselines::default();
        assert!(empty.judge("k:0", "x", "unresolved", None, "t").is_none());
        assert!(empty.judge("k:1", "y", "ambiguous", None, "t").is_none());
        assert!(empty.fresh.is_empty());
    }

    #[test]
    fn a_note_is_one_short_line() {
        let item = |mention: &str, state: &str, relpath: Option<&str>| Flagged {
            mention: mention.into(),
            state: state.into(),
            relpath: relpath.map(str::to_string),
        };
        assert_eq!(note(&[]), None);
        assert_eq!(
            note(&[item("parse", CHANGED, Some("src/wire.py"))]).unwrap(),
            "may be stale: this passage names `parse` (src/wire.py), which changed after the passage was written"
        );
        assert_eq!(
            note(&[item("gone", MISSING, None)]).unwrap(),
            "may be stale: this passage names `gone`, which no longer exists (renamed or deleted)"
        );
        let many = [
            item("a", CHANGED, Some("a.py")),
            item("b", MISSING, None),
            item("c", CHANGED, None),
            item("d", CHANGED, None),
        ];
        let line = note(&many).unwrap();
        assert!(!line.contains('\n'));
        assert!(line.ends_with("`a` (a.py), `b` (no longer defined), `c` and 1 more"));
    }
}
