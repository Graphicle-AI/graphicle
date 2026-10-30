//! Notes first, and one excerpt per file: how `context` (and so
//! `recall_project` and a pushed prompt card) treats a project's notes.
//!
//! Notes are imported as plain documents, so 0.1.1 ranked them for a
//! question in prose only by passage BM25, beside code that shared the
//! question's common words; the same file often took two slots; and a
//! prompt hook that printed the packet could lose excerpts at Claude Code's
//! 10,000-character hook limit.
//!
//! - [`rerank`]: for a question that names no code ([`names_code`]), the
//!   first [`DEPTH`] files of the fused order, one passage each, keep their
//!   places, except that the notes and documents ([`note_path`]) among them
//!   are ordered again among the notes' own places: each note keeps its
//!   fused reciprocal rank and adds how well its file name and folder match
//!   the question's words (Porter stems, so close variants match), its
//!   headings, and its recency among the candidate notes (the latest ISO
//!   date, up to today, in its path or text). A code file never moves, so a
//!   question in prose about code finds its code where the fused ranking
//!   put it. A question that names code keeps the fused ranking unchanged.
//! - [`one_per_file`]: a packet quotes at most one window of a file, and one
//!   copy of identical files (the same bytes under other paths), so the
//!   budget goes to more files.
//!
//! Both apply only with the `notes_first` switch ([`switch`]), which is off
//! unless a call turns it on ([`crate::query::NOTES_FIRST_DEFAULT`]).
use crate::store::s;
use crate::{Error, Result};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

/// The packet field that reports what this module did, and the switch that
/// turns it on.
pub const FIELD: &str = "notes_first";

/// The call's `notes_first` switch, and whether the call left it out (then
/// it is [`crate::query::NOTES_FIRST_DEFAULT`]).
pub fn switch(args: &Value) -> Result<(bool, bool)> {
    match args.get(FIELD) {
        None | Some(Value::Null) => Ok((crate::query::NOTES_FIRST_DEFAULT, true)),
        Some(Value::Bool(on)) => Ok((*on, false)),
        Some(_) => Err(Error::bad("notes_first must be true or false")),
    }
}

/// Files of the fused order [`rerank`] orders again.
pub const DEPTH: usize = 50;
/// Weight of a note's file name and folder that match the question: the
/// stems it matches over the square root of the name's stem count plus one.
pub const NAME_WEIGHT: f64 = 0.02;
/// Weight of headings that match: matched stems over the square root of
/// one plus the headings' length in units of 20 words.
pub const HEADING_WEIGHT: f64 = 0.003;
/// Weight of recency among the candidate notes' distinct dates: the newest
/// adds all of it, the oldest 1/n of it (n dates), the others their share
/// by place, an undated note nothing; with fewer than two dates there is no
/// order, and nothing is added.
pub const RECENCY_WEIGHT: f64 = 0.02;
/// Headings read per document (front matter fields, then `#` lines).
pub const HEADINGS: usize = 12;
/// The fused order's reciprocal-rank constant: rank r (from 0) keeps
/// 1/(RRF_K + r + 1).
const RRF_K: f64 = 60.0;

/// Whether `question` names code: a backtick span, a snake_case,
/// SCREAMING_CASE or camelCase identifier, a PascalCase name of three or
/// more parts, a `Type::member` path, a call `name(...)`, a code file
/// (`parser.py`) or a path to one (`web/app.js`). Names that only look
/// like code do not count: `Acme_Docs`, `macOS`, `next.js`. Such a
/// question keeps the code ranking (and the packet 0.1.1 built for it);
/// only questions in prose rank notes again.
pub fn names_code(question: &str) -> bool {
    static CODE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(concat!(
            r"`[^`\n]+`",
            r"|\b[a-z][a-z0-9]*(?:_[a-z0-9]+)+\b",
            r"|\b[A-Z][A-Z0-9]*(?:_[A-Z0-9]+)+\b",
            // camelCase with two lowercase letters before the capital
            // (`getChoices`; not `iPhone`, `iPad`, `eBay`).
            r"|\b[a-z]{2}[a-z0-9]*[A-Z][a-z0-9]+[A-Za-z0-9]*\b",
            r"|\b[A-Z][a-z0-9]+(?:[A-Z][a-z0-9]+){2,}\b",
            r"|\b[A-Za-z_][A-Za-z0-9_]*::[A-Za-z_]",
            r"|\b[A-Za-z_][A-Za-z0-9_.]*\([^()\n]{0,80}\)",
            r"|\b[A-Za-z0-9_-]+\.(?:rs|py|pyi|go|java|kt|c|h|cc|cpp|hpp|rb|swift|cs|php|scala|sh)\b",
            r"|[A-Za-z0-9_.-]*/[A-Za-z0-9_./-]*\.(?:ts|tsx|js|jsx|mjs|cjs|vue|svelte)\b",
        ))
        .expect("the names-code rule")
    });
    CODE.is_match(question)
}

/// A note or document by its file name: Markdown, plain text,
/// reStructuredText, AsciiDoc, Org, PDF or Word. Not HTML: a folder's
/// `.html` and `.htm` files are kept as markup (no `#` headings), and are
/// pages and templates rather than notes.
pub fn note_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    let Some((_, extension)) = name.rsplit_once('.') else {
        return false;
    };
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "md" | "markdown" | "mdx" | "txt" | "text" | "rst" | "adoc" | "org" | "pdf" | "docx"
    )
}

/// Words a match ignores: question words and common function words.
const COMMON: [&str; 70] = [
    "the", "and", "for", "with", "what", "which", "where", "when", "does", "how", "why", "who",
    "whom", "that", "this", "these", "those", "from", "into", "are", "was", "were", "has", "have",
    "had", "not", "can", "could", "about", "there", "their", "they", "them", "then", "than", "its",
    "our", "your", "you", "all", "any", "did", "should", "would", "will", "may", "might", "must",
    "been", "being", "now", "each", "both", "also", "but", "use", "used", "using", "one", "two",
    "get", "got", "say", "said", "after", "before", "since", "until", "over", "it",
];

/// The Porter stems of `text`'s words that match: at least three characters
/// or a digit, without [`COMMON`] words.
fn stems(text: &str) -> BTreeSet<String> {
    graphicle_memory::text::words(text)
        .into_iter()
        .filter(|w| {
            (w.chars().count() >= 3 || w.chars().any(|c| c.is_ascii_digit()))
                && !COMMON.contains(&w.as_str())
        })
        .map(|w| graphicle_memory::stem::stem(&w))
        .collect()
}

/// A document's name words: its file name without the extension and the
/// name of its folder (`docs/setup/INSTALL-GUIDE.md`: install, guide, setup).
fn name_text(path: &str) -> String {
    let mut parts = path.rsplit('/');
    let file = parts.next().unwrap_or(path);
    let stem = file.rsplit_once('.').map_or(file, |(stem, _)| stem);
    format!("{stem} {}", parts.next().unwrap_or(""))
}

/// A document's headings: the `title`, `name` and `description` fields of
/// a leading front matter block, then its first [`HEADINGS`] Markdown `#`
/// lines. A log or a generated summary with hundreds of headings would
/// match every question; its first headings say what it is.
fn heading_text(content: &str) -> String {
    let mut out = String::new();
    let mut lines = content.lines();
    if content.starts_with("---") {
        lines.next();
        for line in lines.by_ref().take(60) {
            if line.trim_end() == "---" {
                break;
            }
            for key in ["title:", "name:", "description:"] {
                if let Some(value) = line.trim_start().strip_prefix(key) {
                    out.push_str(value);
                    out.push('\n');
                }
            }
        }
    }
    let mut headings = 0;
    for line in lines {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') && trimmed.trim_start_matches('#').starts_with(' ') {
            out.push_str(trimmed.trim_start_matches('#'));
            out.push('\n');
            headings += 1;
            if headings == HEADINGS {
                break;
            }
        }
    }
    out
}

/// The latest ISO date (`2026-10-15`) in a document's path or text, up to
/// `today` (a plan's later dates say nothing of how current a note is):
/// notes that log work name their dates, and the newest says how current
/// it is.
fn newest_date(path: &str, content: &str, today: &str) -> String {
    static DATE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"\b(?:19|20)\d\d-(?:0[1-9]|1[0-2])-(?:0[1-9]|[12]\d|3[01])\b")
            .expect("the date rule")
    });
    DATE.find_iter(path)
        .chain(DATE.find_iter(content))
        .map(|m| m.as_str())
        .filter(|date| *date <= today)
        .max()
        .unwrap_or("")
        .to_string()
}

/// One file of the fused order, as [`rerank`] scores it.
struct Candidate<'a> {
    rank: usize,
    path: &'a str,
    identity: &'a str,
    note: bool,
    score: f64,
    name: usize,
    headings: usize,
    date: String,
}

/// The fused order (`rows`: passages in rank order, memories left out)
/// for a question that names no code, one passage per file: the first
/// [`DEPTH`] files, each with its best-ranked passage. Code files keep their
/// places. The notes ([`note_path`]) are ordered again among the notes'
/// places, by fused reciprocal rank plus [`NAME_WEIGHT`] times the name
/// match, [`HEADING_WEIGHT`] times the heading match and [`RECENCY_WEIGHT`]
/// times the date's place among the candidate notes' dates up to `today`
/// (`YYYY-MM-DD`); equal scores keep the fused order. `None` when the
/// question names code; the report says what was done.
pub fn rerank(
    rows: &[&Value],
    documents: &[Value],
    question: &str,
    today: &str,
) -> (Option<Vec<String>>, Value) {
    if names_code(question) {
        return (None, json!({"status":"code_question"}));
    }
    let terms = stems(question);
    let content: BTreeMap<&str, &str> = documents
        .iter()
        .map(|d| (s(d, "id"), s(d, "content")))
        .collect();
    let mut seen = BTreeSet::new();
    let mut files: Vec<Candidate<'_>> = Vec::new();
    for row in rows {
        let source = &row["source"];
        if !seen.insert(s(source, "id")) {
            continue;
        }
        let path = s(source, "path");
        let rank = files.len();
        let mut file = Candidate {
            rank,
            path,
            identity: s(row, "identity"),
            note: note_path(path),
            score: 1.0 / (RRF_K + rank as f64 + 1.0),
            name: 0,
            headings: 0,
            date: String::new(),
        };
        if file.note {
            let name = stems(&name_text(path));
            file.name = name.intersection(&terms).count();
            file.score += NAME_WEIGHT * file.name as f64 / (1.0 + name.len() as f64).sqrt();
            let text = content.get(s(source, "id")).copied().unwrap_or("");
            let heading = heading_text(text);
            let length = graphicle_memory::text::words(&heading).len() as f64;
            file.headings = stems(&heading).intersection(&terms).count();
            file.score += HEADING_WEIGHT * file.headings as f64 / (1.0 + length / 20.0).sqrt();
            file.date = newest_date(path, text, today);
        }
        files.push(file);
        if files.len() == DEPTH {
            break;
        }
    }
    let mut dates: Vec<&str> = files
        .iter()
        .map(|f| f.date.as_str())
        .filter(|d| !d.is_empty())
        .collect();
    dates.sort_unstable();
    dates.dedup();
    let places: Vec<Option<usize>> = files
        .iter()
        .map(|f| dates.binary_search(&f.date.as_str()).ok())
        .collect();
    if dates.len() > 1 {
        let count = dates.len() as f64;
        for (file, place) in files.iter_mut().zip(places) {
            if let Some(place) = place {
                file.score += RECENCY_WEIGHT * (place + 1) as f64 / count;
            }
        }
    }
    // Code files keep their places; the notes fill the notes' places, best
    // first.
    let places: Vec<usize> = (0..files.len()).filter(|&i| files[i].note).collect();
    let mut notes = places.clone();
    notes.sort_by(|&a, &b| {
        files[b]
            .score
            .total_cmp(&files[a].score)
            .then(files[a].rank.cmp(&files[b].rank))
    });
    let mut order: Vec<usize> = (0..files.len()).collect();
    for (&place, &note) in places.iter().zip(&notes) {
        order[place] = note;
    }
    let moved = order.iter().enumerate().filter(|(i, f)| i != *f).count();
    let report = json!({"status":"reranked","files":files.len(),"notes":places.len(),"notes_moved":moved,
        "first":order.iter().take(3).map(|&i| &files[i]).map(|f| json!({
        "path":f.path,"fused_rank":f.rank + 1,"name_matches":f.name,"heading_matches":f.headings,
        "newest_date":f.date})).collect::<Vec<_>>()});
    (
        Some(
            order
                .iter()
                .map(|&i| files[i].identity.to_string())
                .collect(),
        ),
        report,
    )
}

/// Keep the first window of each file among `candidates` (in rank order)
/// and the first of files with identical bytes; how many windows of an
/// already quoted file, and how many copies, were dropped.
pub fn one_per_file(candidates: &mut Vec<Value>, documents: &[Value]) -> (usize, usize) {
    let sha: BTreeMap<&str, &str> = documents
        .iter()
        .map(|d| (s(d, "id"), s(d, "sha256")))
        .collect();
    let mut files = BTreeSet::new();
    let mut contents = BTreeSet::new();
    let (mut repeated, mut copies) = (0, 0);
    candidates.retain(|c| {
        let id = s(&c["source"], "id").to_string();
        if files.contains(&id) {
            repeated += 1;
            return false;
        }
        if let Some(digest) = sha.get(id.as_str()).filter(|d| !d.is_empty())
            && !contents.insert(digest.to_string())
        {
            copies += 1;
            return false;
        }
        files.insert(id);
        true
    });
    (repeated, copies)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(path: &str, content: &str) -> Value {
        json!({"id":format!("file:{path}"),"path":path,"content":content,
            "sha256":graphicle_memory::text::sha256(content.as_bytes())})
    }
    fn row(path: &str, start: u64, text: &str) -> Value {
        json!({"identity":format!("{path}#{start}"),"kind":"source","text":text,
            "source":{"id":format!("file:{path}"),"path":path,"byte_start":start}})
    }

    #[test]
    fn code_names_keep_the_code_ranking() {
        for q in [
            "Why does `parse_header` fail?",
            "Where is parse_header defined?",
            "Where is getChoices called?",
            "What does Field.get_choices() return?",
            "Who calls Store::project_context?",
            "Fix the bug in src/query.rs",
            "What changed in ModelAdminChecks?",
            "Why is MAX_RANK_LIMIT 60?",
            "What does src/web/app.js render?",
        ] {
            assert!(names_code(q), "{q}");
        }
        for q in [
            "Where is the Windows port plan and the Windows sprint log?",
            "Which GitHub org hosts Graphicle's public repository, and what is the Homebrew install command?",
            "What is the SSH user (build-...) of the test host now?",
            "Which release tag/commit did the maintainers push as the public v0.1.0?",
            "How long should we expect a build machine to live (about 10 h)?",
            "Where is the tooling that moved the old stages into the Acme_Docs repository?",
            "What did the memory rewrite achieve on the Kubernetes and next.js extracts?",
            "Does the iPhone app sync with the iPad and eBay listings?",
            "How should the cache folder be protected from macOS's nightly /tmp cleanup?",
        ] {
            assert!(!names_code(q), "{q}");
        }
    }

    #[test]
    fn notes_are_named_by_file_folder_and_headings() {
        assert!(note_path("notes/build-checklist.md") && note_path("docs/a.rst"));
        assert!(!note_path("src/query.rs") && !note_path("Makefile"));
        assert!(!note_path("templates/index.html") && !note_path("docs/OLD.HTM"));
        assert_eq!(
            stems(&name_text("docs/guides/install-step3/GUIDE.md")),
            stems("GUIDE install-step3")
        );
        let head = heading_text(
            "---\nname: cache-folder-cleaner\ndescription: \"keep verified backups\"\n---\n\n# Status log\nbody words\n## Release plan\n",
        );
        for word in ["cache", "verified", "backups", "status", "release"] {
            assert!(head.to_lowercase().contains(word), "{word}: {head}");
        }
        assert!(!head.contains("body"));
        let many: String = (0..40).map(|i| format!("# heading{i}\n")).collect();
        assert_eq!(heading_text(&many).lines().count(), HEADINGS);
        assert_eq!(
            newest_date(
                "notes/a-2026-10-01.md",
                "seen 2026-10-14, then 2025-01-02, plan 2027-04-17",
                "2026-10-15"
            ),
            "2026-10-14"
        );
    }

    #[test]
    fn prose_reranks_notes_by_name_and_recency_one_passage_per_file() {
        let documents = vec![
            doc("notes/windows-port.md", "# Windows port plan\n2026-10-01"),
            doc("notes/log.md", "# Log\nwindows port 2026-10-14"),
            doc("src/windows_port.rs", "fn windows_port_plan() {}"),
            doc("notes/other.md", "# Other\n2026-09-01"),
        ];
        let rows = [
            row("src/windows_port.rs", 0, "fn windows_port_plan() {}"),
            row("notes/other.md", 0, "other"),
            row("notes/other.md", 50, "other again"),
            row("notes/log.md", 0, "windows port"),
            row("notes/windows-port.md", 0, "Windows port plan"),
        ];
        let refs: Vec<&Value> = rows.iter().collect();
        let (order, report) = rerank(
            &refs,
            &documents,
            "Where is the Windows port plan?",
            "2026-10-15",
        );
        let order = order.expect("reranked");
        // The code file keeps its fused place; among the notes' places the
        // named note comes first, then the newest note, the old note last,
        // once.
        assert_eq!(
            order,
            [
                "src/windows_port.rs#0",
                "notes/windows-port.md#0",
                "notes/log.md#0",
                "notes/other.md#0"
            ],
            "{report}"
        );
        let (none, report) = rerank(
            &refs,
            &documents,
            "Where is `windows_port_plan` defined?",
            "2026-10-15",
        );
        assert!(none.is_none());
        assert_eq!(report["status"], "code_question");
    }

    #[test]
    fn code_files_keep_their_fused_places() {
        // The fused first file is code: neither a test file whose name
        // matches the question nor a newer note moves it; the notes trade
        // places among themselves.
        let documents = vec![
            doc("src/terminal.rs", "fn render() {}"),
            doc(
                "CHANGELOG.md",
                "# Changelog\n## 2026-10-14\nterminal colours",
            ),
            doc("tests/terminal_colours.rs", "fn colours() {}"),
            doc(
                "notes/terminal-colours.md",
                "# Terminal colours\n2026-10-01",
            ),
        ];
        let rows = [
            row("src/terminal.rs", 0, "fn render() {}"),
            row("CHANGELOG.md", 0, "terminal colours"),
            row("tests/terminal_colours.rs", 0, "fn colours() {}"),
            row("notes/terminal-colours.md", 0, "Terminal colours"),
        ];
        let refs: Vec<&Value> = rows.iter().collect();
        let (order, report) = rerank(
            &refs,
            &documents,
            "Why are the terminal colours wrong?",
            "2026-10-15",
        );
        assert_eq!(
            order.expect("reranked"),
            [
                "src/terminal.rs#0",
                "notes/terminal-colours.md#0",
                "tests/terminal_colours.rs#0",
                "CHANGELOG.md#0"
            ],
            "{report}"
        );
        assert_eq!(report["notes"], 2);
        assert_eq!(report["notes_moved"], 2);
    }

    #[test]
    fn one_window_per_file_and_one_copy_of_identical_files() {
        let documents = vec![
            doc("a.md", "same"),
            doc("copy/a.md", "same"),
            doc("b.md", "other"),
        ];
        let hit = |path: &str, start: u64| json!({"source":{"id":format!("file:{path}"),"byte_start":start}});
        let mut candidates = vec![
            hit("a.md", 0),
            hit("a.md", 10),
            hit("copy/a.md", 0),
            hit("b.md", 0),
        ];
        assert_eq!(one_per_file(&mut candidates, &documents), (1, 1));
        let kept: Vec<&str> = candidates.iter().map(|c| s(&c["source"], "id")).collect();
        assert_eq!(kept, ["file:a.md", "file:b.md"]);
    }
}
