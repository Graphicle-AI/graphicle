// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/detect.py
//! Classification, credential filtering, interpreter resolution and noise rules.
use crate::{Result, detect_documents, manifest_ingest, path_syntax};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs::{self, File},
    io::Read,
    path::Path,
    sync::LazyLock,
};
pub static CONSTANTS: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("assets/detect/constants.json")).unwrap());
pub fn strings(key: &str) -> impl Iterator<Item = &'static str> {
    CONSTANTS[key]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
}
pub fn contains(key: &str, value: &str) -> bool {
    strings(key).any(|s| s == value)
}
pub fn py_space(c: char) -> bool {
    c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}
pub fn suffix(path: &Path) -> String {
    path_syntax::suffix(&path.file_name().unwrap_or_default().to_string_lossy()).to_lowercase()
}
pub fn stem(name: &str) -> String {
    let suffix = path_syntax::suffix(name);
    name[..name.len() - suffix.len()].into()
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileType {
    Code,
    Document,
    Paper,
    Image,
    Video,
}
impl FileType {
    pub const ALL: [Self; 5] = [
        Self::Code,
        Self::Document,
        Self::Paper,
        Self::Image,
        Self::Video,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::Document => "document",
            Self::Paper => "paper",
            Self::Image => "image",
            Self::Video => "video",
        }
    }
}
pub fn is_env_template(name: &str) -> bool {
    let name = name.to_lowercase();
    strings("_ENV_TEMPLATE_SUFFIXES").any(|s| name.ends_with(s))
        && (name.starts_with(".env.") || name.starts_with(".envrc."))
}
static KEYWORDS: LazyLock<Vec<fancy_regex::Regex>> = LazyLock::new(|| {
    strings("_GENERIC_KEYWORD_PATTERNS")
        .map(|s| fancy_regex::Regex::new(&format!("(?i){s}")).unwrap())
        .collect()
});
static SENSITIVE: LazyLock<Vec<fancy_regex::Regex>> = LazyLock::new(|| {
    strings("_SENSITIVE_PATTERNS")
        .map(|s| fancy_regex::Regex::new(&format!("(?i){s}")).unwrap())
        .collect()
});
pub fn generic_keyword_hit(name: &str) -> bool {
    let full = stem(name);
    let stripped = full.trim_start_matches('.');
    let stem = if stripped.is_empty() {
        full.as_str()
    } else {
        stripped
    };
    for pattern in KEYWORDS.iter() {
        let mut hit = false;
        for m in pattern.find_iter(stem).flatten() {
            hit = true;
            if m.end() == stem.len() {
                return true;
            }
        }
        if hit
            && stem
                .split(|c: char| matches!(c, '-' | '_' | '.') || py_space(c))
                .filter(|s| !s.is_empty())
                .count()
                <= 2
        {
            return true;
        }
    }
    false
}
pub fn is_prose_note(path: &Path) -> bool {
    if !contains("_PROSE_EXTS", &suffix(path)) {
        return false;
    }
    let full = stem(&path.file_name().unwrap_or_default().to_string_lossy());
    let stripped = full.trim_start_matches('.');
    let stem = if stripped.is_empty() {
        full.as_str()
    } else {
        stripped
    };
    if stem.eq_ignore_ascii_case("tokens") {
        return true;
    }
    !KEYWORDS.iter().any(|p| {
        p.find(stem)
            .ok()
            .flatten()
            .is_some_and(|m| m.start() == 0 && m.end() == stem.len())
    })
}
pub fn is_graphable_source(path: &Path) -> bool {
    classify_file(path) == Some(FileType::Code)
        && !contains("_SECRET_PRONE_DATA_EXTS", &suffix(path))
}
/// The folder names above `path`. Windows: from the path's components, so a
/// `\\`-separated path names its folders (Graphify's `Path.parts`).
fn parent_names(path: &Path) -> Vec<String> {
    #[cfg(windows)]
    {
        path.parent()
            .map(|p| {
                p.components()
                    .filter_map(|c| match c {
                        std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
    #[cfg(not(windows))]
    {
        let mut parts = path_syntax::posix_parts(&crate::fsutil::path_string(path));
        parts.pop();
        parts
    }
}
pub fn is_sensitive(path: &Path) -> bool {
    let parents = parent_names(path);
    if parents
        .iter()
        .any(|p| contains("_CREDENTIAL_STORE_DIRS", &p.to_lowercase()))
    {
        return true;
    }
    if parents
        .iter()
        .any(|p| contains("_AMBIGUOUS_SENSITIVE_DIRS", &p.to_lowercase()))
        && !is_graphable_source(path)
    {
        return true;
    }
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    if SENSITIVE.iter().any(|p| p.is_match(&name).unwrap_or(false)) && !is_env_template(&name) {
        return true;
    }
    generic_keyword_hit(&name) && !(is_graphable_source(path) || is_prose_note(path))
}
static PAPER: LazyLock<Vec<regex::Regex>> = LazyLock::new(|| {
    strings("_PAPER_SIGNALS")
        .map(|p| {
            let insensitive =
                !(p.starts_with(r"\\cite") || p.starts_with(r"\[") || p.starts_with(r"\d"));
            crate::regex_compat::compile(&format!("{}{p}", if insensitive { "(?i)" } else { "" }))
        })
        .collect()
});
pub fn looks_like_paper(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    let Ok(raw) = fs::read(path) else {
        return false;
    };
    let text = detect_documents::decode_ignore(&raw)
        .chars()
        .take(3000)
        .collect::<String>();
    PAPER.iter().filter(|p| p.is_match(&text)).count() >= 3
}
/// POSIX shlex.split(comments=False), including quoted empty arguments.
pub fn shell_split(text: &str) -> Result<Vec<String>> {
    let mut chars = text.chars().peekable();
    let mut token = String::new();
    let mut out = Vec::new();
    let mut quote = None;
    let mut started = false;
    while let Some(c) = chars.next() {
        match quote {
            Some('\'') => {
                if c == '\'' {
                    quote = None;
                } else {
                    token.push(c);
                }
            }
            Some('"') => match c {
                '"' => quote = None,
                '\\' => {
                    let next = chars.next().ok_or("No escaped character")?;
                    if !matches!(next, '"' | '\\') {
                        token.push('\\');
                    }
                    token.push(next);
                }
                _ => token.push(c),
            },
            _ => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    started = true;
                }
                '\\' => {
                    token.push(chars.next().ok_or("No escaped character")?);
                    started = true;
                }
                ' ' | '\t' | '\r' | '\n' => {
                    if started {
                        out.push(std::mem::take(&mut token));
                        started = false;
                    }
                }
                _ => {
                    token.push(c);
                    started = true;
                }
            },
        }
    }
    if quote.is_some() {
        return Err("No closing quotation".into());
    }
    if started {
        out.push(token);
    }
    Ok(out)
}
pub fn split_env_s(value: &str, rest: &[String]) -> Result<Vec<String>> {
    shell_split(
        std::iter::once(value)
            .chain(rest.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
            .trim_matches(py_space),
    )
}
pub fn env_command_args(args: &[String], allow_split: bool) -> Result<Vec<String>> {
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--" {
            return Ok(args[i + 1..].to_vec());
        }
        if allow_split {
            let payload = if arg == "-S" || arg == "-vS" {
                if i + 1 >= args.len() {
                    return Ok(vec![]);
                }
                Some(split_env_s(&args[i + 1..].join(" "), &[])?)
            } else if let Some(s) = arg
                .strip_prefix("-S")
                .filter(|s| !s.is_empty())
                .or_else(|| arg.strip_prefix("-vS").filter(|s| !s.is_empty()))
            {
                Some(split_env_s(s, &args[i + 1..])?)
            } else if let Some(s) = arg.strip_prefix("--split-string=") {
                Some(split_env_s(s, &args[i + 1..])?)
            } else if arg == "--split-string" {
                if i + 1 >= args.len() {
                    return Ok(vec![]);
                }
                Some(split_env_s(&args[i + 1], &args[i + 2..])?)
            } else {
                None
            };
            if let Some(split) = payload {
                return env_command_args(&split, false);
            }
        }
        if ["-u", "-C", "-P", "-a", "--unset", "--chdir", "--argv0"].contains(&arg) {
            if i + 2 > args.len() {
                return Ok(vec![]);
            }
            i += 2;
            continue;
        }
        if ["-u", "-C", "-P", "-a"].iter().any(|p| arg.starts_with(p))
            && arg.len() > 2
            && !arg.starts_with("--")
        {
            i += 1;
            continue;
        }
        if ["--unset=", "--chdir=", "--argv0="]
            .iter()
            .any(|p| arg.starts_with(p))
            || [
                "-",
                "-i",
                "-0",
                "-v",
                "--ignore-environment",
                "--null",
                "--debug",
                "--list-signal-handling",
            ]
            .contains(&arg)
            || ["--default-signal", "--ignore-signal", "--block-signal"]
                .iter()
                .any(|p| arg.starts_with(p))
        {
            i += 1;
            continue;
        }
        if arg.starts_with('-') {
            return Ok(vec![]);
        }
        if arg.contains('=') {
            i += 1;
            continue;
        }
        return Ok(args[i..].to_vec());
    }
    Ok(vec![])
}
pub fn shebang_interpreter(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let mut first = vec![0; 256];
    let n = file.read(&mut first).ok()?;
    first.truncate(n);
    if !first.starts_with(b"#!") {
        return None;
    }
    let end = first
        .iter()
        .position(|b| *b == b'\n')
        .unwrap_or(first.len());
    let line = String::from_utf8_lossy(&first[2..end]);
    let parts = shell_split(line.trim_matches(py_space)).ok()?;
    let mut interpreter = Path::new(parts.first()?)
        .file_name()?
        .to_string_lossy()
        .into_owned();
    if interpreter == "env" {
        let args = env_command_args(&parts[1..], true).ok()?;
        interpreter = Path::new(args.first()?)
            .file_name()?
            .to_string_lossy()
            .into_owned();
    }
    Some(strip_interpreter_version(interpreter))
}
/// `_strip_interpreter_version` (Graphify 0.9.93, #4535): a shebang names a
/// specific version as often as the generic one (`python3.12`, `bash5`,
/// `ruby3.2`). A known code interpreter is kept; otherwise the name with its
/// major version (`python3`), then the bare name (`lua`), whichever is known;
/// else the original.
pub fn strip_interpreter_version(interpreter: String) -> String {
    static VERSIONED: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"^([A-Za-z]+)(\d+)(?:\.\d+)*$").unwrap());
    if contains("_SHEBANG_CODE_INTERPRETERS", &interpreter) {
        return interpreter;
    }
    if let Some(m) = VERSIONED.captures(&interpreter) {
        for candidate in [format!("{}{}", &m[1], &m[2]), m[1].to_owned()] {
            if contains("_SHEBANG_CODE_INTERPRETERS", &candidate) {
                return candidate;
            }
        }
    }
    interpreter
}
pub fn shebang_file_type(path: &Path) -> Option<FileType> {
    shebang_interpreter(path)
        .filter(|s| contains("_SHEBANG_CODE_INTERPRETERS", s))
        .map(|_| FileType::Code)
}
pub fn classify_file(path: &Path) -> Option<FileType> {
    if manifest_ingest::is_package_manifest_path(path)
        || path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().to_lowercase().ends_with(".blade.php"))
    {
        return Some(FileType::Code);
    }
    // A project may declare what an ambiguous extension means to it
    // (`.graphiclerc` `language.inc=php`, 0.9.87 #2961); classify by it.
    let ext = crate::rcfile::effective_suffix(path).to_lowercase();
    if ext.is_empty() {
        return shebang_file_type(path);
    }
    if contains("CODE_EXTENSIONS", &ext) {
        return Some(FileType::Code);
    }
    if contains("PAPER_EXTENSIONS", &ext) {
        if path.components().any(|c| {
            strings("_ASSET_DIR_MARKERS").any(|s| c.as_os_str().to_string_lossy().ends_with(s))
        }) {
            return None;
        }
        return Some(FileType::Paper);
    }
    if contains("IMAGE_EXTENSIONS", &ext) {
        return Some(FileType::Image);
    }
    if contains("DOC_EXTENSIONS", &ext) {
        return Some(if looks_like_paper(path) {
            FileType::Paper
        } else {
            FileType::Document
        });
    }
    if contains("OFFICE_EXTENSIONS", &ext)
        || crate::google_workspace::GOOGLE_WORKSPACE_EXTENSIONS.contains(&ext.as_str())
    {
        return Some(FileType::Document);
    }
    contains("VIDEO_EXTENSIONS", &ext).then_some(FileType::Video)
}
pub fn has_coverage_artifacts(path: &Path) -> bool {
    strings("_COVERAGE_ARTIFACT_FILES").any(|s| path.join(s).is_file())
        || strings("_COVERAGE_ARTIFACT_DIRS").any(|s| path.join(s).is_dir())
}
pub fn has_build_output_markers(path: &Path) -> bool {
    if path.join("production").is_dir() || path.join("_next").is_dir() {
        return true;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return false;
    };
    let mut subdirs = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            return false;
        };
        let path = entry.path();
        if path.is_file()
            && strings("_BUILD_OUTPUT_SUFFIXES").any(|s| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .to_lowercase()
                    .ends_with(s)
            })
        {
            return true;
        }
        if path.is_dir() {
            subdirs.push(path);
        }
    }
    let mut budget = 4000;
    for sub in subdirs {
        if budget == 0 {
            break;
        }
        let Ok(entries) = fs::read_dir(sub) else {
            return false;
        };
        for entry in entries {
            let Ok(entry) = entry else {
                return false;
            };
            budget -= 1;
            if entry.path().is_file()
                && strings("_BUILD_OUTPUT_SUFFIXES").any(|s| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .to_lowercase()
                        .ends_with(s)
                })
            {
                return true;
            }
            if budget == 0 {
                break;
            }
        }
    }
    false
}
pub fn has_venv_markers(path: &Path) -> bool {
    path.join("pyvenv.cfg").is_file()
        || path.join("bin/activate").is_file()
        || path.join("Scripts/activate").is_file()
        || path.join("conda-meta").is_dir()
        || fs::read_dir(path.join("lib")).is_ok_and(|entries| {
            entries
                .flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("python"))
        })
}
/// Platforms whose project-scope skills dir sits one level below the hidden
/// dir (`.pi/agent/skills`, `.config/kilo/skills`).
const NESTED_SKILL_HOLDERS: [(&str, &str); 2] = [(".pi", "agent"), (".config", "kilo")];

/// Single files `install` writes whole into a project (Graphify 0.9.76,
/// #4057), matched on the hidden holder dir plus the exact relative path and
/// never on the bare file name. Upstream's own `graphify.*` names stay listed
/// so a project the third-party tool configured is not indexed either.
const INSTALLED_FILES: [[&str; 3]; 14] = [
    [".agents", "rules", "graphicle.md"],
    [".agents", "workflows", "graphicle.md"],
    [".cursor", "rules", "graphicle.mdc"],
    [".kilo", "plugins", "graphicle.js"],
    [".kiro", "steering", "graphicle.md"],
    [".opencode", "plugins", "graphicle.js"],
    [".windsurf", "rules", "graphicle.md"],
    [".agents", "rules", "graphify.md"],
    [".agents", "workflows", "graphify.md"],
    [".cursor", "rules", "graphify.mdc"],
    [".kilo", "plugins", "graphify.js"],
    [".kiro", "steering", "graphify.md"],
    [".opencode", "plugins", "graphify.js"],
    [".windsurf", "rules", "graphify.md"],
];

fn dir_name(path: Option<&Path>) -> String {
    path.and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// True for a single file `install` wrote into the project (or upstream's).
pub fn is_installed_rule_file(path: &Path) -> bool {
    let parts = path
        .components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>();
    parts.len() >= 3
        && INSTALLED_FILES.iter().any(|file| {
            file.iter()
                .zip(&parts[parts.len() - 3..])
                .all(|(a, b)| a == b)
        })
}

pub fn is_noise_dir(part: &str, parent: Option<&Path>) -> bool {
    if contains("_SKIP_DIRS", part) {
        return true;
    }
    // The installed skill folder as `install --project` writes it (Graphify
    // 0.9.76, #4057): `<hidden dir>/skills/graphicle/`,
    // `.pi/agent/skills/graphicle/`, `.config/kilo/skills/graphicle/` and
    // `.aider/graphicle/`, and upstream's `graphify` folders in the same
    // places; the bare names stay indexed (#2479).
    if (part == "graphicle" || part == "graphify")
        && let Some(parent) = parent
    {
        let parent_name = dir_name(Some(parent));
        if parent_name == ".aider" {
            return true;
        }
        if parent_name == "skills" {
            let holder = parent.parent();
            let holder_name = dir_name(holder);
            if holder_name.starts_with('.') && holder_name != ".." {
                return true;
            }
            let grand = dir_name(holder.and_then(Path::parent));
            return NESTED_SKILL_HOLDERS.contains(&(grand.as_str(), holder_name.as_str()));
        }
    }
    if part == "env" || part == ".env" || part.ends_with("_env") {
        return parent.is_some_and(|p| has_venv_markers(&p.join(part)));
    }
    if part == "coverage" {
        return parent.is_some_and(|p| has_coverage_artifacts(&p.join(part)));
    }
    if part == "out" {
        return parent.is_some_and(|p| has_build_output_markers(&p.join(part)));
    }
    if part == "snapshots" {
        return parent.is_some_and(|p| {
            contains(
                "_JS_SNAPSHOT_TEST_ROOTS",
                &p.file_name().unwrap_or_default().to_string_lossy(),
            ) || fs::read_dir(p.join(part)).is_ok_and(|entries| {
                entries
                    .flatten()
                    .any(|e| e.file_name().to_string_lossy().ends_with(".snap"))
            })
        });
    }
    part.ends_with("_venv")
        || part.ends_with(".egg-info")
        || (part == "worktrees"
            && parent.is_some_and(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with('.'))
            }))
}

#[cfg(all(test, windows))]
mod windows_sensitive_tests {
    use std::path::Path;

    #[test]
    fn credential_folders_match_backslash_paths() {
        assert!(super::is_sensitive(Path::new(r"C:\repo\.aws\notes.txt")));
        assert!(super::is_sensitive(Path::new(r"C:\repo\.SSH\notes.txt")));
        assert!(!super::is_sensitive(Path::new(r"C:\repo\docs\notes.txt")));
    }
}
