//! Graphicle Core's size limits: generous defaults for a
//! tool that runs on the user's machine, each settable by whoever runs
//! Graphicle with a `limits.<name>` key in the configuration file
//! (`models.json` or `--config`) or a `GRAPHICLE_MAX_<NAME>` environment
//! variable, which wins over the file. Requests never carry limits, so on a
//! shared server an API caller cannot raise them.
//!
//! - The project limits (`project_files`, `project_bytes`) refuse a sync,
//!   with a message that names the count, the limit, the setting that raises
//!   it and how to leave paths out.
//! - The per-file limits never fail a sync. A text file over `file_bytes` is
//!   left out and listed. A file over an analysis limit (`analysis_bytes`,
//!   `file_calls`, `file_symbols`, `file_nodes`) keeps its text indexed and
//!   searchable; its structure analysis stops at the limit, and the sync
//!   result lists it.
//! - The document, transcript and connector limits refuse (or, for one
//!   connector document, skip and report) with a message that names the
//!   limit and its setting.
use crate::{Error, Result};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// The limits in effect. [`Limits::default`] are Graphicle Core's defaults.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Text files per project.
    pub project_files: usize,
    /// Bytes of text per project.
    pub project_bytes: usize,
    /// Bytes of one text file that is indexed; a larger one is left out.
    pub file_bytes: usize,
    /// Bytes of one file that gets structure analysis; a larger one is
    /// searchable as text only.
    pub analysis_bytes: usize,
    /// Calls collected from one file.
    pub file_calls: usize,
    /// Declarations collected from one file (and imports and document links,
    /// each list).
    pub file_symbols: usize,
    /// Syntax nodes walked in one file.
    pub file_nodes: usize,
    /// Bytes of one document (PDF, Office, EPUB, HTML, image) read.
    pub document_bytes: usize,
    /// Bytes of text from one document: extracted from a file, or one
    /// connector or uploaded document.
    pub document_text_bytes: usize,
    /// Bytes an Office or EPUB archive may expand to.
    pub archive_bytes: usize,
    /// Entries an Office or EPUB archive may hold.
    pub archive_entries: usize,
    /// Bytes of documents one folder sync reads in all.
    pub folder_document_bytes: usize,
    /// Bytes of document originals a project keeps until `storage.prune`.
    pub retained_original_bytes: usize,
    /// Bytes of one agent session transcript imported or captured.
    pub transcript_bytes: usize,
    /// Bytes of text one connector's snapshot holds.
    pub connector_snapshot_bytes: usize,
    /// Documents a connector source lists (also Drive folders walked and
    /// the comments of one Jira issue).
    pub connector_documents: usize,
    /// Messages of one Slack thread and blocks of one Notion page.
    pub connector_items: usize,
    /// Structure nodes of one Jira document.
    pub connector_nodes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            project_files: 100_000,
            project_bytes: GIB,
            file_bytes: 8 * MIB,
            analysis_bytes: 4 * MIB,
            file_calls: 50_000,
            file_symbols: 16_000,
            file_nodes: 2_000_000,
            // Earlier builds: 16 MiB, 512 KiB, 64 MiB, 5,000, 256 MiB,
            // 256 MiB, 64 MiB, 64 MiB, 5,000, 2,000 and 50,000.
            document_bytes: 128 * MIB,
            document_text_bytes: 8 * MIB,
            archive_bytes: 512 * MIB,
            archive_entries: 40_000,
            folder_document_bytes: 2 * GIB,
            retained_original_bytes: 2 * GIB,
            transcript_bytes: 512 * MIB,
            connector_snapshot_bytes: 512 * MIB,
            connector_documents: 40_000,
            connector_items: 16_000,
            connector_nodes: 400_000,
        }
    }
}

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;
const GIB: usize = 1024 * MIB;

/// Every limit: its name (the configuration key under `limits`), whether it
/// counts bytes (and so takes a unit), and the largest value accepted.
pub const SETTINGS: [(&str, bool, usize); 18] = [
    ("project_files", false, 10_000_000),
    ("project_bytes", true, 1024 * GIB),
    ("file_bytes", true, 512 * MIB),
    ("analysis_bytes", true, 512 * MIB),
    ("file_calls", false, 10_000_000),
    ("file_symbols", false, 10_000_000),
    ("file_nodes", false, 1_000_000_000),
    ("document_bytes", true, 4 * GIB),
    ("document_text_bytes", true, 512 * MIB),
    ("archive_bytes", true, 16 * GIB),
    ("archive_entries", false, 10_000_000),
    ("folder_document_bytes", true, 1024 * GIB),
    ("retained_original_bytes", true, 1024 * GIB),
    ("transcript_bytes", true, 16 * GIB),
    ("connector_snapshot_bytes", true, 64 * GIB),
    ("connector_documents", false, 10_000_000),
    ("connector_items", false, 10_000_000),
    ("connector_nodes", false, 100_000_000),
];

/// The environment variable of the limit `name`: `GRAPHICLE_MAX_<NAME>`.
pub fn variable(name: &str) -> String {
    format!("GRAPHICLE_MAX_{}", name.to_ascii_uppercase())
}

/// The most text a sync request may carry, and the most manifest entries:
/// internal batch sizes; a local sync sends larger projects in parts.
pub const REQUEST_FILES: usize = 5000;
pub const REQUEST_BYTES: usize = 64 * MIB;
/// The most files one `sync.upload` request carries.
pub const UPLOAD_FILES: usize = 1000;
/// The limits the command line resolved from its configuration, for the
/// code paths that have no store at hand (document conversion, connectors,
/// transcripts). Set once per process.
static INSTALLED: std::sync::OnceLock<Limits> = std::sync::OnceLock::new();

/// Make `limits` the process's limits (the command line, after it loads
/// the configuration file).
pub fn install(limits: Limits) {
    let _ = INSTALLED.set(limits);
}

/// The process's limits: the installed ones, else the environment over the
/// defaults, read once (a bad variable is reported where the configuration
/// is loaded; here it leaves the defaults).
pub fn current() -> Limits {
    static ENVIRONMENT: std::sync::OnceLock<Limits> = std::sync::OnceLock::new();
    match INSTALLED.get() {
        Some(limits) => *limits,
        None => *ENVIRONMENT
            .get_or_init(|| Limits::from_environment(&BTreeMap::new()).unwrap_or_default()),
    }
}

impl Limits {
    fn slot(&mut self, name: &str) -> Option<&mut usize> {
        Some(match name {
            "project_files" => &mut self.project_files,
            "project_bytes" => &mut self.project_bytes,
            "file_bytes" => &mut self.file_bytes,
            "analysis_bytes" => &mut self.analysis_bytes,
            "file_calls" => &mut self.file_calls,
            "file_symbols" => &mut self.file_symbols,
            "file_nodes" => &mut self.file_nodes,
            "document_bytes" => &mut self.document_bytes,
            "document_text_bytes" => &mut self.document_text_bytes,
            "archive_bytes" => &mut self.archive_bytes,
            "archive_entries" => &mut self.archive_entries,
            "folder_document_bytes" => &mut self.folder_document_bytes,
            "retained_original_bytes" => &mut self.retained_original_bytes,
            "transcript_bytes" => &mut self.transcript_bytes,
            "connector_snapshot_bytes" => &mut self.connector_snapshot_bytes,
            "connector_documents" => &mut self.connector_documents,
            "connector_items" => &mut self.connector_items,
            "connector_nodes" => &mut self.connector_nodes,
            _ => return None,
        })
    }

    /// The defaults, then the configuration file's `limits` (`config`), then
    /// the environment (`variable` reads one variable). A value that is not
    /// a whole number from 1 to the limit's maximum, or an unknown key, is
    /// an error that names the setting.
    pub fn resolve(
        config: &BTreeMap<String, Value>,
        variable_value: impl Fn(&str) -> Option<String>,
    ) -> Result<Self> {
        let mut limits = Self::default();
        for (name, value) in config {
            let Some(&(_, bytes, max)) = SETTINGS.iter().find(|(known, ..)| known == name) else {
                return Err(Error::bad(format!(
                    "Unknown setting limits.{name} in the configuration file; the limits are {}",
                    names()
                )));
            };
            let parsed = match value {
                Value::Number(number) => number.as_u64().and_then(|n| usize::try_from(n).ok()),
                Value::String(text) => parse(text, bytes),
                _ => None,
            };
            *limits.slot(name).expect("a listed limit") =
                checked(parsed, max).ok_or_else(|| {
                    invalid(
                        &format!("limits.{name} in the configuration file"),
                        value,
                        bytes,
                        max,
                    )
                })?;
        }
        for (name, bytes, max) in SETTINGS {
            let key = variable(name);
            let Some(raw) = variable_value(&key).filter(|v| !v.trim().is_empty()) else {
                continue;
            };
            *limits.slot(name).expect("a listed limit") = checked(parse(&raw, bytes), max)
                .ok_or_else(|| invalid(&key, &json!(raw), bytes, max))?;
        }
        Ok(limits)
    }

    /// The value of the limit `name`; `None` for an unknown name.
    pub fn get(&self, name: &str) -> Option<usize> {
        let mut copy = *self;
        copy.slot(name).map(|value| *value)
    }

    /// Every limit in effect, by name: its `value`, the value `shown` to
    /// people (`100,000`, `1 GiB`) and whether it differs from the default
    /// (`set`), for `graphicle config show`.
    pub fn view(&self) -> Value {
        let defaults = Self::default();
        let mut view = serde_json::Map::new();
        for (name, bytes, _) in SETTINGS {
            let value = self.get(name).unwrap_or_default();
            let shown = if bytes { size(value) } else { grouped(value) };
            view.insert(
                name.to_string(),
                json!({"value":value,"shown":shown,"set":Some(value) != defaults.get(name)}),
            );
        }
        Value::Object(view)
    }

    /// [`Limits::resolve`] with this process's environment.
    pub fn from_environment(config: &BTreeMap<String, Value>) -> Result<Self> {
        Self::resolve(config, |name| {
            std::env::var_os(name).map(|v| v.to_string_lossy().into_owned())
        })
    }

    /// The per-file structure analysis limits.
    pub fn analysis(&self) -> graphicle_memory::structure::Limits {
        graphicle_memory::structure::Limits {
            bytes: self.analysis_bytes,
            calls: self.file_calls,
            symbols: self.file_symbols,
            nodes: self.file_nodes,
        }
    }

    /// The source text a project may keep, earlier versions included, until
    /// `storage.prune`: four times [`Limits::project_bytes`], at least 2 GiB.
    pub fn retained_bytes(&self) -> usize {
        self.project_bytes.saturating_mul(4).max(2 * GIB)
    }

    /// The limits as the native `analyze` request takes them.
    pub fn analysis_request(&self) -> Value {
        json!({"bytes":self.analysis_bytes,"calls":self.file_calls,
            "symbols":self.file_symbols,"nodes":self.file_nodes})
    }
}

fn names() -> String {
    let names: Vec<_> = SETTINGS.iter().map(|(name, ..)| *name).collect();
    format!(
        "{} and {}",
        names[..names.len() - 1].join(", "),
        names[names.len() - 1]
    )
}

fn checked(value: Option<usize>, max: usize) -> Option<usize> {
    value.filter(|n| (1..=max).contains(n))
}

/// A whole number, with a `KiB`, `MiB`, `GiB` or `TiB` unit when `bytes`.
fn parse(text: &str, bytes: bool) -> Option<usize> {
    let text = text.trim();
    let digits = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(digits);
    let number: usize = number.parse().ok()?;
    let factor = match unit.trim().to_ascii_lowercase().as_str() {
        "" => 1,
        "kib" if bytes => KIB,
        "mib" if bytes => MIB,
        "gib" if bytes => GIB,
        "tib" if bytes => 1024 * GIB,
        _ => return None,
    };
    number.checked_mul(factor)
}

fn invalid(setting: &str, value: &Value, bytes: bool, max: usize) -> Error {
    let shown = match value {
        Value::String(text) => format!("\"{text}\""),
        other => other.to_string(),
    };
    if bytes {
        Error::bad(format!(
            "Invalid {setting}: {shown}. Use a number of bytes from 1 to {}, optionally with KiB, MiB, GiB or TiB (for example 2GiB)",
            size(max)
        ))
    } else {
        Error::bad(format!(
            "Invalid {setting}: {shown}. Use a whole number from 1 to {}",
            grouped(max)
        ))
    }
}

/// `12345678` as `12,345,678`.
pub fn grouped(number: usize) -> String {
    let digits = number.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// What one question reads at most: the memory crate's chunking bounds,
/// [`graphicle_memory::ingest::MAX_DOCUMENTS`] documents and
/// [`graphicle_memory::ingest::MAX_PASSAGES`] passages of up to 2,600
/// characters (about 100 MB of text).
pub const QUESTION_BYTES: usize = graphicle_memory::ingest::MAX_PASSAGES * 2600;

/// The note a sync result carries (`question_limits`) when the project is
/// larger than one question reads: past it, each question reads the files
/// that match it best. `None` within the bounds.
pub fn question_note(files: usize, bytes: usize) -> Option<String> {
    let documents = graphicle_memory::ingest::MAX_DOCUMENTS;
    (files > documents || bytes > QUESTION_BYTES).then(|| {
        format!(
            "This project has {} text files and {} of text; one question reads at most {} files and {} passages (about 100 MB of text), the ones that match the question best. Use include to choose the files a question reads",
            grouped(files),
            size(bytes),
            grouped(documents),
            grouped(graphicle_memory::ingest::MAX_PASSAGES)
        )
    })
}

/// How to raise the limit `name`, for refusals: `limits.<name> in the
/// configuration file or GRAPHICLE_MAX_<NAME>`.
pub fn raise(name: &str) -> String {
    format!(
        "limits.{name} in the configuration file or {}",
        variable(name)
    )
}

/// A size for people: `1 GiB`, `1.5 GiB`, `512 KiB`, `900 bytes`.
pub fn size(bytes: usize) -> String {
    for (unit, scale) in [
        ("TiB", 1024 * GIB),
        ("GiB", GIB),
        ("MiB", MIB),
        ("KiB", KIB),
    ] {
        if bytes >= scale {
            let value = bytes as f64 / scale as f64;
            return if bytes.is_multiple_of(scale) {
                format!("{} {unit}", bytes / scale)
            } else {
                format!("{value:.1} {unit}")
            };
        }
    }
    format!("{bytes} bytes")
}

/// The refusal of a project over a project limit: `files` text files with
/// `bytes` of text, counted exactly or, past the limit, as `about` so many.
pub fn project_refusal(files: usize, bytes: usize, about: bool, limits: &Limits) -> Error {
    let about = if about { "about " } else { "" };
    let mut found = Vec::new();
    let mut settings = Vec::new();
    if files > limits.project_files {
        found.push(format!(
            "{about}{} text files (the limit is {})",
            grouped(files),
            grouped(limits.project_files)
        ));
        settings.push("project_files");
    }
    if bytes > limits.project_bytes {
        found.push(format!(
            "{about}{} of text (the limit is {})",
            size(bytes),
            size(limits.project_bytes)
        ));
        settings.push("project_bytes");
    }
    let keys: Vec<String> = settings
        .iter()
        .map(|name| format!("limits.{name}"))
        .collect();
    let variables: Vec<String> = settings.iter().map(|name| variable(name)).collect();
    let (limit, variable_noun) = if settings.len() > 1 {
        ("limits", "environment variables")
    } else {
        ("limit", "environment variable")
    };
    Error::bad(format!(
        "This project is too large to sync: it has {}. Raise the {limit} with {} in the configuration file (models.json) or the {} {variable_noun}, or leave untracked paths out with .gitignore or .git/info/exclude (they cover only files Git does not track; graphicle config set sync.tracked_ignored false, or GRAPHICLE_SYNC_TRACKED_IGNORED=0, leaves out the tracked files they match too), or set up a subfolder: graphicle setup path/to/subfolder",
        found.join(" and "),
        keys.join(" and "),
        variables.join(" and ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(config: Value, environment: &[(&str, &str)]) -> Result<Limits> {
        let config: BTreeMap<String, Value> = serde_json::from_value(config).unwrap();
        Limits::resolve(&config, |name| {
            environment
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        })
    }

    #[test]
    fn the_defaults_are_graphicle_cores() {
        let limits = resolve(json!({}), &[]).unwrap();
        assert_eq!(limits, Limits::default());
        assert_eq!(limits.project_files, 100_000);
        assert_eq!(limits.project_bytes, 1 << 30);
        assert_eq!(limits.file_bytes, 8 << 20);
        assert_eq!(limits.analysis_bytes, 4 << 20);
        assert_eq!(limits.file_calls, 50_000);
        assert_eq!(limits.file_symbols, 16_000);
        assert_eq!(limits.file_nodes, 2_000_000);
        assert_eq!(limits.retained_bytes(), 4 << 30);
        assert_eq!(limits.document_bytes, 128 << 20);
        assert_eq!(limits.document_text_bytes, 8 << 20);
        assert_eq!(limits.archive_bytes, 512 << 20);
        assert_eq!(limits.archive_entries, 40_000);
        assert_eq!(limits.folder_document_bytes, 2 << 30);
        assert_eq!(limits.retained_original_bytes, 2 << 30);
        assert_eq!(limits.transcript_bytes, 512 << 20);
        assert_eq!(limits.connector_snapshot_bytes, 512 << 20);
        assert_eq!(limits.connector_documents, 40_000);
        assert_eq!(limits.connector_items, 16_000);
        assert_eq!(limits.connector_nodes, 400_000);
        assert_eq!(
            raise("transcript_bytes"),
            "limits.transcript_bytes in the configuration file or GRAPHICLE_MAX_TRANSCRIPT_BYTES"
        );
        assert_eq!(
            limits.analysis(),
            graphicle_memory::structure::Limits::default()
        );
    }

    #[test]
    fn the_file_sets_limits_and_the_environment_wins() {
        let limits = resolve(
            json!({"project_files":200000,"project_bytes":"2GiB","file_calls":"75000",
                "transcript_bytes":"1GiB","connector_documents":50000}),
            &[
                ("GRAPHICLE_MAX_DOCUMENT_TEXT_BYTES", "32MiB"),
                ("GRAPHICLE_MAX_PROJECT_FILES", "300000"),
                ("GRAPHICLE_MAX_FILE_BYTES", " 16 MiB "),
                ("GRAPHICLE_MAX_FILE_NODES", ""),
            ],
        )
        .unwrap();
        assert_eq!(limits.project_files, 300_000);
        assert_eq!(limits.project_bytes, 2 << 30);
        assert_eq!(limits.file_bytes, 16 << 20);
        assert_eq!(limits.file_calls, 75_000);
        assert_eq!(limits.file_nodes, 2_000_000, "an empty variable is unset");
        assert_eq!(limits.transcript_bytes, 1 << 30);
        assert_eq!(limits.connector_documents, 50_000);
        assert_eq!(limits.document_text_bytes, 32 << 20);
        for (name, ..) in SETTINGS {
            assert_eq!(
                variable(name),
                format!("GRAPHICLE_MAX_{}", name.to_uppercase())
            );
        }
    }

    #[test]
    fn bad_values_name_the_setting_and_the_accepted_range() {
        let message = |config: Value, environment: &[(&str, &str)]| {
            resolve(config, environment).unwrap_err().message
        };
        assert_eq!(
            message(json!({"project_files":0}), &[]),
            "Invalid limits.project_files in the configuration file: 0. Use a whole number from 1 to 10,000,000"
        );
        assert_eq!(
            message(json!({}), &[("GRAPHICLE_MAX_PROJECT_BYTES", "1TB")]),
            "Invalid GRAPHICLE_MAX_PROJECT_BYTES: \"1TB\". Use a number of bytes from 1 to 1 TiB, optionally with KiB, MiB, GiB or TiB (for example 2GiB)"
        );
        assert!(
            message(json!({"project_file":5}), &[])
                .starts_with("Unknown setting limits.project_file in the configuration file; the limits are project_files, project_bytes,")
        );
        for bad in [
            json!({"file_calls":"10MiB"}),
            json!({"file_calls":-1}),
            json!({"file_calls":1.5}),
            json!({"file_calls":true}),
            json!({"file_nodes":2_000_000_000u64}),
            json!({"file_bytes":"513MiB"}),
            json!({"document_text_bytes":"513MiB"}),
            json!({"connector_items":0}),
            json!({"project_files":"1e5"}),
        ] {
            assert!(resolve(bad.clone(), &[]).is_err(), "{bad}");
        }
        assert!(resolve(json!({}), &[("GRAPHICLE_MAX_FILE_CALLS", "many")]).is_err());
        assert!(
            resolve(
                json!({}),
                &[("GRAPHICLE_MAX_PROJECT_BYTES", "99999999999999999999GiB")]
            )
            .is_err()
        );
    }

    #[test]
    fn a_refused_project_names_the_count_the_limit_the_setting_and_the_way_out() {
        let limits = Limits {
            project_files: 10,
            ..Limits::default()
        };
        let refused = project_refusal(12_345, 2048, true, &limits).message;
        assert_eq!(
            refused,
            "This project is too large to sync: it has about 12,345 text files (the limit is 10). Raise the limit with limits.project_files in the configuration file (models.json) or the GRAPHICLE_MAX_PROJECT_FILES environment variable, or leave untracked paths out with .gitignore or .git/info/exclude (they cover only files Git does not track; graphicle config set sync.tracked_ignored false, or GRAPHICLE_SYNC_TRACKED_IGNORED=0, leaves out the tracked files they match too), or set up a subfolder: graphicle setup path/to/subfolder"
        );
        let both = project_refusal(11, 3 << 30, false, &limits).message;
        assert!(both.contains(
            "it has 11 text files (the limit is 10) and 3 GiB of text (the limit is 1 GiB)"
        ));
        assert!(both.contains("limits.project_files and limits.project_bytes"));
        assert!(
            both.contains("Raise the limits with limits.project_files and limits.project_bytes")
        );
        assert!(both.contains(
            "the GRAPHICLE_MAX_PROJECT_FILES and GRAPHICLE_MAX_PROJECT_BYTES environment variables,"
        ));
        assert_eq!(question_note(50_000, 99 << 20), None);
        assert_eq!(
            question_note(60_000, 64 << 20).unwrap(),
            "This project has 60,000 text files and 64 MiB of text; one question reads at most 50,000 files and 40,000 passages (about 100 MB of text), the ones that match the question best. Use include to choose the files a question reads"
        );
        assert!(question_note(10, 200 << 20).is_some());
        assert_eq!(size(1536 << 20), "1.5 GiB");
        assert_eq!(size(900), "900 bytes");
        assert_eq!(grouped(1_000), "1,000");
        assert_eq!(grouped(999), "999");
    }
}
