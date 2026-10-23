//! The passage index of a project's files, built when the project syncs, so
//! that `context` looks a question up instead of chunking and scoring every
//! file of the project for every question (GitHub issue #8).
//!
//! **What it holds.** Each current file (`repo_heads`) is laid out in the
//! passages `context` ranks ([`CHARACTERS`] characters and [`TOKENS`]
//! cl100k tokens each, as `query::passages` lays them out), and each passage
//! is counted three ways:
//! - the Porter-stemmed words of its title and text, which `rank`'s BM25
//!   scores (`retrieval::rank` with `stem`, the default);
//! - the stemmed words of its `user:` and of its `assistant:` turns, which
//!   `rank`'s speaker arm scores (`speaker: "auto"`, the default);
//! - its ASCII identifiers of three or more characters, case-sensitive,
//!   which the identifier arm matches (`crate::identifier_arm`).
//!
//! The counts go into segments of postings (`am_passage_terms`: one row per
//! segment and term, the passages that hold it and how often). A sync adds
//! one segment for the files that are new or changed since the last one and
//! drops the entries of the files that changed or went away, so it reads
//! only those files ([`update`]); a later sync merges the segments when
//! there are many or when removed files fill them ([`MAX_SEGMENTS`],
//! [`MAX_DEAD`]). A store opened by an older program keeps working: it never
//! reads these tables, and the files it syncs are checked against the index
//! by path and SHA-256 when a question is asked ([`Index::open`]).
//!
//! **Exactness.** The statistics a question reads (the number of passages,
//! each term's passages and the average length) are those of the whole
//! catalogue, as when every passage is chunked and scored, so a question
//! within the old per-question bounds ranks exactly as before; past them it
//! reads every passage instead of the ones its words selected.
use crate::{Error, Result, now};
use graphicle_memory::{cache, ingest, retrieval, stem::Stemmer};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// The layout and counting rules a segment was built with; a segment of
/// another format is rebuilt by the next sync and never read.
pub const FORMAT: i64 = 1;
/// Characters and tokens of one passage, as `query::passages` lays them out.
pub const CHARACTERS: usize = 2600;
pub const TOKENS: usize = 640;
/// Files one segment holds; a first build of a large project writes several.
const SEGMENT_FILES: usize = 20_000;
/// Segments past this are merged into one by the next sync.
pub const MAX_SEGMENTS: usize = 8;
/// Segments past this are merged by a refresh before an answer too, which
/// otherwise never waits for a merge.
pub const MAX_SEGMENTS_ANSWERING: usize = 4 * MAX_SEGMENTS;
/// When the files that changed or went away hold more than this share of
/// the whole index's passages, the next sync merges it. A segment none of
/// whose files is current is dropped by any update, which is cheap.
pub const MAX_DEAD: f64 = 0.3;
/// Shortest and longest identifiers counted (the identifier arm's names have
/// three or more characters).
const IDENT_MIN: usize = 3;
const IDENT_MAX: usize = 128;
/// Term prefixes: the speaker arm's turns and the identifier arm's names.
pub const USER: &str = "\u{1}u";
pub const ASSISTANT: &str = "\u{1}a";
pub const IDENT: &str = "\u{2}";

/// Creates the index tables. Called when a store opens.
pub(crate) fn migrate(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS am_passage_segments (
            repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
            segment INTEGER NOT NULL, format INTEGER NOT NULL,
            files INTEGER NOT NULL, passages INTEGER NOT NULL, created_at TEXT NOT NULL,
            PRIMARY KEY(repository_id,segment));
         CREATE TABLE IF NOT EXISTS am_passage_files (
            repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
            path TEXT NOT NULL, sha256 TEXT NOT NULL,
            segment INTEGER NOT NULL, ordinal INTEGER NOT NULL, spans BLOB NOT NULL,
            PRIMARY KEY(repository_id,path));
         CREATE TABLE IF NOT EXISTS am_passage_terms (
            repository_id TEXT NOT NULL REFERENCES repo_projects(id) ON DELETE CASCADE,
            segment INTEGER NOT NULL, term TEXT NOT NULL,
            postings BLOB NOT NULL, PRIMARY KEY(repository_id,segment,term)) WITHOUT ROWID;",
    )?;
    Ok(())
}

/// The tables that hold a project's passage index (verified deletion).
pub const TABLES: [&str; 3] = [
    "am_passage_terms",
    "am_passage_files",
    "am_passage_segments",
];

fn put(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn get(buffer: &[u8], position: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    let mut shift = 0;
    loop {
        let byte = *buffer.get(*position)?;
        *position += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

/// One passage of an indexed file: its bytes and lines, and the number of
/// words `rank` counts in it, in its `user:` turns and in its `assistant:`
/// turns (0 when it has none).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Passage {
    pub start: usize,
    pub end: usize,
    pub line: usize,
    pub end_line: usize,
    pub length: usize,
    pub user: usize,
    pub assistant: usize,
}

fn encode_passages(passages: &[Passage]) -> Vec<u8> {
    let mut out = Vec::with_capacity(passages.len() * 10);
    put(&mut out, passages.len() as u64);
    for p in passages {
        for value in [
            p.start,
            p.end,
            p.line,
            p.end_line,
            p.length,
            p.user,
            p.assistant,
        ] {
            put(&mut out, value as u64);
        }
    }
    out
}

fn decode_passages(blob: &[u8]) -> Option<Vec<Passage>> {
    let mut position = 0;
    let count = get(blob, &mut position)? as usize;
    let mut passages = Vec::with_capacity(count.min(1 << 20));
    for _ in 0..count {
        let mut next = || get(blob, &mut position).map(|v| v as usize);
        passages.push(Passage {
            start: next()?,
            end: next()?,
            line: next()?,
            end_line: next()?,
            length: next()?,
            user: next()?,
            assistant: next()?,
        });
    }
    (position == blob.len()).then_some(passages)
}

/// The speaker roles of the speaker arm's two sides, as `rank` keeps them.
fn side(role: &str) -> BTreeSet<String> {
    BTreeSet::from([role.to_string()])
}

/// The ASCII identifiers of `text` (maximal runs of letters, digits and
/// `_`, as a whole-word match with ASCII word boundaries reads them) of
/// [`IDENT_MIN`] to [`IDENT_MAX`] characters, with their counts. A run that
/// starts with a digit is left out: the identifier arm's names start with a
/// letter or `_` (`identifier_arm::named`), so none is ever looked up.
pub fn identifiers(text: &str) -> BTreeMap<&str, u32> {
    let bytes = text.as_bytes();
    let mut found: BTreeMap<&str, u32> = BTreeMap::new();
    let mut start = None;
    for (i, &byte) in bytes.iter().chain(std::iter::once(&b' ')).enumerate() {
        let word = byte.is_ascii_alphanumeric() || byte == b'_';
        match (word, start) {
            (true, None) => start = Some(i),
            (false, Some(from)) => {
                let length = i - from;
                if (IDENT_MIN..=IDENT_MAX).contains(&length) && !bytes[from].is_ascii_digit() {
                    // ASCII bytes only: a char boundary on both sides.
                    *found.entry(&text[from..i]).or_default() += 1;
                }
                start = None;
            }
            _ => {}
        }
    }
    found
}

/// One file laid out and counted: its passages and, per term, the passages
/// that hold it with their counts, in passage order.
struct Analysed {
    path: String,
    sha: String,
    passages: Vec<Passage>,
    terms: HashMap<String, Vec<(u32, u32)>>,
}

/// Count `count` of `term` in passage `index` of a file's `terms`.
fn count_term(terms: &mut HashMap<String, Vec<(u32, u32)>>, term: &str, index: u32, count: u32) {
    match terms.get_mut(term) {
        Some(list) => list.push((index, count)),
        None => {
            terms.insert(term.to_string(), vec![(index, count)]);
        }
    }
}

/// Whether `text` may hold a turn of `role` (`retrieval::speaker_text` keeps
/// a role's turns from a line whose label, trimmed and in lower case, is
/// the role): the role's name, in any ASCII case.
fn may_speak(text: &str, role: &str) -> bool {
    let (text, role) = (text.as_bytes(), role.as_bytes());
    text.len() >= role.len()
        && text
            .windows(role.len())
            .any(|window| window.eq_ignore_ascii_case(role))
}

fn analyse(path: &str, sha: &str, content: &str, stemmer: &mut Stemmer) -> Result<Analysed> {
    let spans = ingest::spans_of(content, CHARACTERS, TOKENS).map_err(Error::from)?;
    let user = side("user");
    let assistant = side("assistant");
    let mut passages = Vec::with_capacity(spans.len());
    let mut terms: HashMap<String, Vec<(u32, u32)>> = HashMap::new();
    let mut term = String::new();
    for (index, span) in spans.iter().enumerate() {
        let text = &content[span.start..span.end];
        let index = index as u32;
        // `rank` counts a row's title (a file's path) and text together.
        let main = cache::stemmed_of(&format!("{path} {text}"), stemmer);
        for (word, count) in main.words.iter() {
            count_term(&mut terms, word, index, count as u32);
        }
        let mut turns = [0usize; 2];
        for (slot, (keep, role, prefix)) in
            [(&user, "user", USER), (&assistant, "assistant", ASSISTANT)]
                .into_iter()
                .enumerate()
        {
            if !may_speak(text, role) {
                continue;
            }
            let spoken = retrieval::speaker_text(text, keep);
            if spoken.is_empty() {
                continue;
            }
            let features = cache::stemmed_of(&spoken, stemmer);
            if features.length == 0 {
                continue;
            }
            turns[slot] = features.length;
            for (word, count) in features.words.iter() {
                term.clear();
                term.push_str(prefix);
                term.push_str(word);
                count_term(&mut terms, &term, index, count as u32);
            }
        }
        for (name, count) in identifiers(text) {
            term.clear();
            term.push_str(IDENT);
            term.push_str(name);
            count_term(&mut terms, &term, index, count);
        }
        passages.push(Passage {
            start: span.start,
            end: span.end,
            line: span.line,
            end_line: span.end_line,
            length: main.length,
            user: turns[0],
            assistant: turns[1],
        });
    }
    Ok(Analysed {
        path: path.to_string(),
        sha: sha.to_string(),
        passages,
        terms,
    })
}

/// Postings of one term in one segment: (ordinal delta, passage, count)
/// varints, by ordinal then passage.
#[derive(Default)]
struct Postings {
    bytes: Vec<u8>,
    last: Option<u32>,
}

impl Postings {
    fn push(&mut self, ordinal: u32, passage: u32, count: u32) {
        let delta = match self.last {
            Some(last) => ordinal - last,
            None => ordinal,
        };
        put(&mut self.bytes, u64::from(delta));
        put(&mut self.bytes, u64::from(passage));
        put(&mut self.bytes, u64::from(count));
        self.last = Some(ordinal);
    }
}

/// Decode a postings blob: `(ordinal, passage, count)` in order.
fn each_posting(blob: &[u8], mut f: impl FnMut(u32, u32, u32)) -> Result<()> {
    let mut position = 0;
    let mut ordinal = 0u64;
    let mut first = true;
    while position < blob.len() {
        let corrupt = || Error::bad("The passage index is corrupt; sync the project again");
        let delta = get(blob, &mut position).ok_or_else(corrupt)?;
        let passage = get(blob, &mut position).ok_or_else(corrupt)?;
        let count = get(blob, &mut position).ok_or_else(corrupt)?;
        ordinal = if first { delta } else { ordinal + delta };
        first = false;
        f(ordinal as u32, passage as u32, count as u32);
    }
    Ok(())
}

/// What [`update`] did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Updated {
    /// Files laid out and counted.
    pub files: usize,
    pub passages: usize,
    /// Files whose entries were dropped (changed or gone).
    pub dropped: usize,
    /// Segments written, segments dropped because none of their files is
    /// current, and whether the index was merged into one.
    pub segments: usize,
    pub emptied: usize,
    pub merged: bool,
    /// A first build was left for a later sync (`full: false`).
    pub deferred: bool,
    pub seconds: f64,
}

impl Updated {
    pub fn report(&self) -> Value {
        json!({"files":self.files,"passages":self.passages,"dropped":self.dropped,
            "segments":self.segments,"emptied":self.emptied,"merged":self.merged,
            "deferred":self.deferred,"seconds":(self.seconds * 10.0).round() / 10.0})
    }
}

/// One segment of a project's index: (segment, format, files, passages).
type Segment = (i64, i64, i64, i64);

/// The segments of a project's index.
fn segments(db: &Connection, repository: &str) -> Result<Vec<Segment>> {
    let mut statement = db.prepare_cached(
        "SELECT segment,format,files,passages FROM am_passage_segments WHERE repository_id=? ORDER BY segment",
    )?;
    let found = statement
        .query_map([repository], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(found)
}

/// Whether the database has `table`.
fn has_table(db: &Connection, table: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?)",
        [table],
        |r| r.get(0),
    )?)
}

/// Remove a project's whole index: the rows of each index table the
/// database has. A backup taken before the index existed, which offboarding
/// scrubs as it is, has none of them (where they exist, the project's rows
/// also go with it by foreign key).
pub fn clear(db: &Connection, repository: &str) -> Result<()> {
    for table in TABLES {
        if has_table(db, table)? {
            db.execute(
                &format!("DELETE FROM {table} WHERE repository_id=?"),
                [repository],
            )?;
        }
    }
    Ok(())
}

/// Bring a project's index up to date with its current files: drop the
/// entries of files that changed or went away, then lay out and count the
/// new and changed files into a new segment, and merge the segments when
/// [`MAX_SEGMENTS`] or [`MAX_DEAD`] says so. With `full` false (a refresh
/// before an answer) a project without an index is left for the next sync,
/// so an answer never waits for a first build. Reads its files from the
/// store; never fails a sync (the caller reports the error).
pub fn update(store: &crate::store::Store, repository: &str, full: bool) -> Result<Updated> {
    let started = std::time::Instant::now();
    let mut updated = Updated::default();
    // What the index holds against the current files.
    let (heads, indexed, existing) = store.read(|db| {
        let heads = crate::store::heads(db, repository)?;
        let mut indexed: HashMap<String, (String, i64)> = HashMap::new();
        let mut statement = db.prepare_cached(
            "SELECT path,sha256,segment FROM am_passage_files WHERE repository_id=?",
        )?;
        let found = statement.query_map([repository], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        for row in found {
            let (path, sha, segment) = row?;
            indexed.insert(path, (sha, segment));
        }
        Ok((heads, indexed, segments(db, repository)?))
    })?;
    let stale_format = existing.iter().any(|(_, format, ..)| *format != FORMAT);
    if !full && (existing.is_empty() || stale_format) && !heads.is_empty() {
        updated.deferred = true;
        return Ok(updated);
    }
    let rebuild = stale_format;
    let needed: Vec<(String, String)> = heads
        .iter()
        .filter(|(path, sha)| rebuild || indexed.get(*path).is_none_or(|(had, _)| had != *sha))
        .map(|(path, sha)| (path.clone(), sha.clone()))
        .collect();
    let dropped: Vec<String> = indexed
        .iter()
        .filter(|(path, (sha, _))| rebuild || heads.get(*path) != Some(sha))
        .map(|(path, _)| path.clone())
        .collect();
    updated.dropped = dropped.len();
    if rebuild {
        store.transaction(|db| clear(db, repository))?;
    } else if !dropped.is_empty() {
        store.transaction(|db| {
            let mut delete =
                db.prepare_cached("DELETE FROM am_passage_files WHERE repository_id=? AND path=?")?;
            for path in &dropped {
                delete.execute(params![repository, path])?;
            }
            Ok(())
        })?;
    }
    for batch in needed.chunks(SEGMENT_FILES) {
        let analysed = analyse_batch(store, repository, batch)?;
        updated.files += analysed.len();
        updated.passages += analysed.iter().map(|a| a.passages.len()).sum::<usize>();
        write_segment(store, repository, &analysed)?;
        updated.segments += 1;
    }
    // Segments none of whose files is current go; then merge when the
    // segments pile up, or when the files that changed or went away hold
    // more than MAX_DEAD of the whole index's passages (a sync only: a
    // refresh before an answer merges only past MAX_SEGMENTS_ANSWERING).
    let (empty, merge) = store.read(|db| {
        let segments = segments(db, repository)?;
        let mut live: HashMap<i64, i64> = HashMap::new();
        let mut statement =
            db.prepare_cached("SELECT segment,spans FROM am_passage_files WHERE repository_id=?")?;
        let found = statement.query_map([repository], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
        })?;
        for row in found {
            let (segment, spans) = row?;
            let mut position = 0;
            let count = get(&spans, &mut position).unwrap_or(0) as i64;
            *live.entry(segment).or_default() += count;
        }
        let (empty, kept): (Vec<&Segment>, Vec<&Segment>) = segments
            .iter()
            .partition(|(segment, ..)| !live.contains_key(segment));
        let empty: Vec<i64> = empty.into_iter().map(|(segment, ..)| *segment).collect();
        let all: i64 = kept.iter().map(|(.., passages)| *passages).sum();
        let alive: i64 = live.values().sum();
        let dead = if all > 0 {
            1.0 - alive as f64 / all as f64
        } else {
            0.0
        };
        let merge = if full {
            kept.len() > MAX_SEGMENTS || (kept.len() > 1 && dead > MAX_DEAD)
        } else {
            kept.len() > MAX_SEGMENTS_ANSWERING
        };
        Ok((empty, merge))
    })?;
    if merge {
        merge_segments(store, repository)?;
        updated.merged = true;
    } else if !empty.is_empty() {
        store.transaction(|db| {
            for segment in &empty {
                for table in ["am_passage_terms", "am_passage_segments"] {
                    db.execute(
                        &format!("DELETE FROM {table} WHERE repository_id=? AND segment=?"),
                        params![repository, segment],
                    )?;
                }
            }
            Ok(())
        })?;
        updated.emptied = empty.len();
    }
    updated.seconds = started.elapsed().as_secs_f64();
    Ok(updated)
}

/// Lay out and count `files` (path, sha256), reading their text from the
/// store, side by side.
fn analyse_batch(
    store: &crate::store::Store,
    repository: &str,
    files: &[(String, String)],
) -> Result<Vec<Analysed>> {
    let mut analysed = Vec::with_capacity(files.len());
    // Read and count in groups, so a large project's text is never all held.
    for group in files.chunks(2_000) {
        let contents = store.read(|db| {
            let mut load = db.prepare_cached(
                "SELECT content FROM repo_objects WHERE repository_id=? AND sha256=?",
            )?;
            let mut contents = Vec::with_capacity(group.len());
            for (path, sha) in group {
                let content: Option<String> = load
                    .query_row(params![repository, sha], |r| r.get(0))
                    .optional()?;
                contents.push((path.as_str(), sha.as_str(), content.unwrap_or_default()));
            }
            Ok(contents)
        })?;
        let done = cache::par_map(
            &contents,
            Stemmer::default,
            |stemmer, (path, sha, content)| analyse(path, sha, content, stemmer),
        );
        for one in done {
            analysed.push(one?);
        }
    }
    Ok(analysed)
}

fn write_segment(
    store: &crate::store::Store,
    repository: &str,
    analysed: &[Analysed],
) -> Result<()> {
    let mut postings: HashMap<&str, Postings> = HashMap::new();
    for (ordinal, file) in analysed.iter().enumerate() {
        for (term, list) in &file.terms {
            let entry = postings.entry(term.as_str()).or_default();
            for &(passage, count) in list {
                entry.push(ordinal as u32, passage, count);
            }
        }
    }
    // In key order: SQLite then appends to its tree instead of inserting
    // across it.
    let mut postings: Vec<(&str, Postings)> = postings.into_iter().collect();
    postings.sort_unstable_by(|a, b| a.0.cmp(b.0));
    let passages: usize = analysed.iter().map(|a| a.passages.len()).sum();
    store.transaction(|db| {
        let segment: i64 = db.query_row(
            "SELECT coalesce(max(segment),0)+1 FROM am_passage_segments WHERE repository_id=?",
            [repository],
            |r| r.get(0),
        )?;
        db.execute(
            "INSERT INTO am_passage_segments(repository_id,segment,format,files,passages,created_at) VALUES(?,?,?,?,?,?)",
            params![repository, segment, FORMAT, analysed.len() as i64, passages as i64, now()],
        )?;
        let mut file = db.prepare_cached(
            "INSERT OR REPLACE INTO am_passage_files(repository_id,path,sha256,segment,ordinal,spans) VALUES(?,?,?,?,?,?)",
        )?;
        for (ordinal, one) in analysed.iter().enumerate() {
            file.execute(params![
                repository,
                one.path,
                one.sha,
                segment,
                ordinal as i64,
                encode_passages(&one.passages)
            ])?;
        }
        let mut term = db.prepare_cached(
            "INSERT INTO am_passage_terms(repository_id,segment,term,postings) VALUES(?,?,?,?)",
        )?;
        for (word, list) in &postings {
            term.execute(params![repository, segment, *word, list.bytes])?;
        }
        Ok(())
    })
}

/// Merge every segment into one: the live files' postings, renumbered, in
/// path order. Reads the postings, never the files' text.
fn merge_segments(store: &crate::store::Store, repository: &str) -> Result<()> {
    store.transaction(|db| merge_on(db, repository))
}

/// [`merge_segments`] in the caller's transaction, when the project has an
/// index (`storage.prune`, so the terms of files that went away go too).
pub(crate) fn merge_on(db: &Connection, repository: &str) -> Result<()> {
    if !TABLES
        .iter()
        .all(|table| has_table(db, table).unwrap_or(false))
        || segments(db, repository)?.is_empty()
    {
        return Ok(());
    }
    let (files, terms) = {
        let mut files: Vec<FileRow> = rows_files(db, repository)?;
        files.sort_by(|a, b| a.0.cmp(&b.0));
        let mut statement = db.prepare_cached(
            "SELECT segment,term,postings FROM am_passage_terms WHERE repository_id=?",
        )?;
        let found = statement.query_map([repository], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Vec<u8>>(2)?,
            ))
        })?;
        let mut terms: BTreeMap<String, Vec<(i64, Vec<u8>)>> = BTreeMap::new();
        for row in found {
            let (segment, term, postings) = row?;
            terms.entry(term).or_default().push((segment, postings));
        }
        (files, terms)
    };
    // (segment, ordinal) of each live file → its new ordinal.
    let renumber: HashMap<(i64, u32), u32> = files
        .iter()
        .enumerate()
        .map(|(new, (_, _, segment, ordinal, _))| ((*segment, *ordinal as u32), new as u32))
        .collect();
    let mut merged: Vec<(String, Vec<u8>)> = Vec::with_capacity(terms.len());
    for (term, lists) in terms {
        let mut entries: Vec<(u32, u32, u32)> = Vec::new();
        for (segment, blob) in &lists {
            each_posting(blob, |ordinal, passage, count| {
                if let Some(&new) = renumber.get(&(*segment, ordinal)) {
                    entries.push((new, passage, count));
                }
            })?;
        }
        if entries.is_empty() {
            continue;
        }
        entries.sort_unstable();
        let mut postings = Postings::default();
        for (ordinal, passage, count) in entries {
            postings.push(ordinal, passage, count);
        }
        merged.push((term, postings.bytes));
    }
    let passages: i64 = files
        .iter()
        .map(|(.., spans)| {
            let mut position = 0;
            get(spans, &mut position).unwrap_or(0) as i64
        })
        .sum();
    clear(db, repository)?;
    db.execute(
        "INSERT INTO am_passage_segments(repository_id,segment,format,files,passages,created_at) VALUES(?,1,?,?,?,?)",
        params![repository, FORMAT, files.len() as i64, passages, now()],
    )?;
    let mut file = db.prepare_cached(
        "INSERT INTO am_passage_files(repository_id,path,sha256,segment,ordinal,spans) VALUES(?,?,?,1,?,?)",
    )?;
    for (ordinal, (path, sha, _, _, spans)) in files.iter().enumerate() {
        file.execute(params![repository, path, sha, ordinal as i64, spans])?;
    }
    let mut term = db.prepare_cached(
        "INSERT INTO am_passage_terms(repository_id,segment,term,postings) VALUES(?,1,?,?)",
    )?;
    for (word, blob) in &merged {
        term.execute(params![repository, word, blob])?;
    }
    Ok(())
}

/// One `am_passage_files` row: path, SHA-256, segment, ordinal and spans.
type FileRow = (String, String, i64, i64, Vec<u8>);

fn rows_files(db: &Connection, repository: &str) -> Result<Vec<FileRow>> {
    let mut statement = db.prepare_cached(
        "SELECT path,sha256,segment,ordinal,spans FROM am_passage_files WHERE repository_id=?",
    )?;
    let found = statement
        .query_map([repository], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(found)
}

/// One indexed file a question reads: its path, SHA-256 and passages.
#[derive(Clone, Debug)]
pub struct File {
    pub path: String,
    pub sha: String,
    pub passages: Vec<Passage>,
}

/// A project's index as one question reads it: the indexed files that are
/// current (their path and SHA-256 are the project's) and the files it
/// lacks, laid out and counted here ([`Index::add_files`]), in path order,
/// and where each one's postings are.
pub struct Index {
    pub files: Vec<File>,
    /// (segment, ordinal) → index into `files`, current files only.
    slots: HashMap<(i64, u32), usize>,
    segments: Vec<i64>,
    repository: String,
    /// Postings of the files added here, by term: (file, passage, count).
    added: HashMap<String, Vec<(usize, u32, u32)>>,
}

/// How an index covers a project's current files.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Coverage {
    /// Current files the index holds.
    pub indexed: usize,
    /// Current files it does not hold (new or changed since its last
    /// update), by path.
    pub missing: Vec<String>,
    /// Segments, and whether any was built with another format.
    pub segments: usize,
    pub stale_format: bool,
}

impl Coverage {
    /// Whether the index exists in this program's format.
    pub fn exists(&self) -> bool {
        self.segments > 0 && !self.stale_format
    }
}

/// How `heads` (path → SHA-256) are covered by the index, without reading
/// its postings: what `doctor` and `setup` report.
pub fn coverage(
    db: &Connection,
    repository: &str,
    heads: &BTreeMap<String, String>,
) -> Result<Coverage> {
    let segments = segments(db, repository)?;
    let mut statement =
        db.prepare_cached("SELECT path,sha256 FROM am_passage_files WHERE repository_id=?")?;
    let indexed: HashMap<String, String> = statement
        .query_map([repository], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut coverage = Coverage {
        segments: segments.len(),
        stale_format: segments.iter().any(|(_, format, ..)| *format != FORMAT),
        ..Coverage::default()
    };
    for (path, sha) in heads {
        if indexed.get(path) == Some(sha) {
            coverage.indexed += 1;
        } else {
            coverage.missing.push(path.clone());
        }
    }
    Ok(coverage)
}

impl Index {
    /// The index of `repository` for its current files `heads`: `None`
    /// when it has none in this program's format. The caller reads the
    /// files the coverage lists as missing itself.
    pub fn open(
        db: &Connection,
        repository: &str,
        heads: &BTreeMap<String, String>,
    ) -> Result<Option<(Self, Coverage)>> {
        let segments = segments(db, repository)?;
        if segments.is_empty() || segments.iter().any(|(_, format, ..)| *format != FORMAT) {
            return Ok(None);
        }
        let mut found = rows_files(db, repository)?;
        found.sort_by(|a, b| a.0.cmp(&b.0));
        let mut files = Vec::with_capacity(found.len());
        let mut slots = HashMap::with_capacity(found.len());
        let mut covered = BTreeSet::new();
        for (path, sha, segment, ordinal, spans) in found {
            if heads.get(&path) != Some(&sha) {
                continue;
            }
            let passages = decode_passages(&spans).ok_or_else(|| {
                Error::bad("The passage index is corrupt; sync the project again")
            })?;
            slots.insert((segment, ordinal as u32), files.len());
            covered.insert(path.clone());
            files.push(File {
                path,
                sha,
                passages,
            });
        }
        let missing: Vec<String> = heads
            .keys()
            .filter(|path| !covered.contains(*path))
            .cloned()
            .collect();
        let coverage = Coverage {
            indexed: files.len(),
            missing,
            segments: segments.len(),
            stale_format: false,
        };
        Ok(Some((
            Self {
                files,
                slots,
                segments: segments.iter().map(|(segment, ..)| *segment).collect(),
                repository: repository.to_string(),
                added: HashMap::new(),
            },
            coverage,
        )))
    }

    /// Lay out and count `files` (path, SHA-256, text) the index lacks, here,
    /// as a sync would, and read them with the indexed ones.
    pub fn add_files(&mut self, files: Vec<(String, String, String)>) -> Result<()> {
        if files.is_empty() {
            return Ok(());
        }
        let done = cache::par_map(&files, Stemmer::default, |stemmer, (path, sha, content)| {
            analyse(path, sha, content, stemmer)
        });
        let mut analysed = Vec::with_capacity(done.len());
        for one in done {
            analysed.push(one?);
        }
        // Every file in path order again: the indexed ones keep their
        // postings through `slots`, the added ones through `added`.
        let before = self.files.len();
        let mut all: Vec<(File, Option<usize>, Option<usize>)> = std::mem::take(&mut self.files)
            .into_iter()
            .enumerate()
            .map(|(old, file)| (file, Some(old), None))
            .collect();
        for (added, one) in analysed.iter().enumerate() {
            all.push((
                File {
                    path: one.path.clone(),
                    sha: one.sha.clone(),
                    passages: one.passages.clone(),
                },
                None,
                Some(added),
            ));
        }
        all.sort_by(|a, b| a.0.path.cmp(&b.0.path));
        let mut moved = vec![0usize; before];
        let mut placed = vec![0usize; analysed.len()];
        for (new, (_, old, added)) in all.iter().enumerate() {
            if let Some(old) = old {
                moved[*old] = new;
            }
            if let Some(added) = added {
                placed[*added] = new;
            }
        }
        for file in self.slots.values_mut() {
            *file = moved[*file];
        }
        for list in self.added.values_mut() {
            for entry in list.iter_mut() {
                entry.0 = moved[entry.0];
            }
        }
        for (added, one) in analysed.into_iter().enumerate() {
            for (term, list) in one.terms {
                let entry = self.added.entry(term).or_default();
                for (passage, count) in list {
                    entry.push((placed[added], passage, count));
                }
            }
        }
        self.files = all.into_iter().map(|(file, ..)| file).collect();
        Ok(())
    }

    /// The current passages that hold `term` (as stored: a stemmed word, or
    /// one of [`USER`], [`ASSISTANT`] and [`IDENT`] before it): (file,
    /// passage, count), by file then passage.
    pub fn postings(&self, db: &Connection, term: &str) -> Result<Vec<(usize, u32, u32)>> {
        let mut statement = db.prepare_cached(
            "SELECT postings FROM am_passage_terms WHERE repository_id=? AND segment=? AND term=?",
        )?;
        let mut found = Vec::new();
        for segment in &self.segments {
            let blob: Option<Vec<u8>> = statement
                .query_row(params![self.repository, segment, term], |r| r.get(0))
                .optional()?;
            if let Some(blob) = blob {
                each_posting(&blob, |ordinal, passage, count| {
                    if let Some(&file) = self.slots.get(&(*segment, ordinal)) {
                        found.push((file, passage, count));
                    }
                })?;
            }
        }
        if let Some(added) = self.added.get(term) {
            found.extend_from_slice(added);
        }
        found.sort_unstable();
        Ok(found)
    }

    /// The index of the file at `path`, if it is current.
    pub fn file(&self, path: &str) -> Option<usize> {
        self.files
            .binary_search_by(|file| file.path.as_str().cmp(path))
            .ok()
    }

    /// Passages, and the sum of their lengths, over the current files: all
    /// words, and the passages with `user:` or `assistant:` turns.
    pub fn totals(&self) -> Totals {
        let mut totals = Totals::default();
        for file in &self.files {
            for p in &file.passages {
                totals.passages += 1;
                totals.length += p.length as u64;
                if p.user > 0 {
                    totals.user_rows += 1;
                    totals.user_length += p.user as u64;
                }
                if p.assistant > 0 {
                    totals.assistant_rows += 1;
                    totals.assistant_length += p.assistant as u64;
                }
            }
        }
        totals
    }
}

/// [`Index::totals`].
#[derive(Clone, Copy, Debug, Default)]
pub struct Totals {
    pub passages: u64,
    pub length: u64,
    pub user_rows: u64,
    pub user_length: u64,
    pub assistant_rows: u64,
    pub assistant_length: u64,
}

/// A project's index summary for `status`, `doctor` and `setup`.
pub fn summary(db: &Connection, repository: &str) -> Result<Value> {
    let heads = crate::store::heads(db, repository)?;
    let coverage = coverage(db, repository, &heads)?;
    let bytes: i64 = db
        .query_row(
            "SELECT coalesce(sum(length(postings)),0) FROM am_passage_terms WHERE repository_id=?",
            [repository],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let state = if !coverage.exists() {
        "missing"
    } else if coverage.missing.is_empty() {
        "ready"
    } else {
        "stale"
    };
    Ok(
        json!({"state":state,"files":heads.len(),"indexed":coverage.indexed,
        "missing":coverage.missing.len(),"segments":coverage.segments,"postings_bytes":bytes}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_and_passages_round_trip() {
        let passages = vec![
            Passage {
                start: 0,
                end: 2600,
                line: 1,
                end_line: 80,
                length: 400,
                user: 0,
                assistant: 3,
            },
            Passage {
                start: 2600,
                end: 3000,
                line: 80,
                end_line: 99,
                length: 1 << 20,
                user: 7,
                assistant: 0,
            },
        ];
        assert_eq!(decode_passages(&encode_passages(&passages)), Some(passages));
        let mut postings = Postings::default();
        postings.push(0, 0, 3);
        postings.push(0, 2, 1);
        postings.push(5, 1, 300);
        let mut seen = Vec::new();
        each_posting(&postings.bytes, |o, p, c| seen.push((o, p, c))).unwrap();
        assert_eq!(seen, [(0, 0, 3), (0, 2, 1), (5, 1, 300)]);
    }

    #[test]
    fn identifiers_are_whole_ascii_runs() {
        let found =
            identifiers("Store::open(cfg); parse_config x9 é_ab é1 abcé xyz_ 404 9lives x86_64");
        let names: Vec<(&str, u32)> = found.into_iter().collect();
        assert_eq!(
            names,
            [
                ("Store", 1),
                ("_ab", 1),
                ("abc", 1),
                ("cfg", 1),
                ("open", 1),
                ("parse_config", 1),
                ("x86_64", 1),
                ("xyz_", 1)
            ]
        );
    }
}
