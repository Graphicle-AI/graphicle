// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/detect.py
//! Anchored ignore rules, tracked-file exemptions and reusable path predicates.
use crate::{
    Result, detect_documents, detect_types, fsutil, io_compat, path_syntax, paths, process,
};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{LazyLock, Mutex},
    time::Duration,
};
pub type Patterns = Vec<(PathBuf, String)>;
pub type MatchCache = HashMap<PathBuf, bool>;
#[derive(Default)]
pub struct EncodingWarnings {
    pub warned: HashSet<String>,
    pub messages: Vec<String>,
}
impl EncodingWarnings {
    pub fn read(&mut self, path: &Path) -> Result<String> {
        let raw = fs::read(path).map_err(|e| io_compat::read_error(&e, path))?;
        let utf8 = raw.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&raw);
        if let Ok(text) = std::str::from_utf8(utf8) {
            return Ok(text.into());
        }
        if raw.starts_with(b"\xff\xfe") || raw.starts_with(b"\xfe\xff") {
            let le = raw.starts_with(b"\xff\xfe");
            if raw.len() % 2 == 0 {
                let units = raw[2..]
                    .chunks_exact(2)
                    .map(|b| {
                        if le {
                            u16::from_le_bytes([b[0], b[1]])
                        } else {
                            u16::from_be_bytes([b[0], b[1]])
                        }
                    })
                    .collect::<Vec<_>>();
                if let Ok(text) = String::from_utf16(&units) {
                    return Ok(text);
                }
            }
        }
        let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
            .iter()
            .filter_map(|key| std::env::var(key).ok())
            .find(|s| !s.is_empty())
            .unwrap_or_default();
        let fallback = locale
            .split_once('.')
            .map(|(_, v)| v.split('@').next().unwrap_or(v))
            .unwrap_or(if cfg!(windows) {
                "windows-1252"
            } else {
                "UTF-8"
            });
        let decoded = encoding_rs::Encoding::for_label(fallback.as_bytes()).and_then(|encoding| {
            let (text, _, errors) = encoding.decode(&raw);
            (!errors).then(|| text.into_owned())
        });
        let (text, encoding) = if let Some(text) = decoded {
            (text, fallback)
        } else {
            (raw.iter().map(|b| *b as char).collect(), "latin-1")
        };
        if self.warned.insert(fsutil::path_string(path)) {
            self.messages.push(format!("[graphicle] WARNING: {} is not valid UTF-8; read it as {encoding} instead. Re-save it as UTF-8 — patterns with non-ASCII characters may not match as written.",fsutil::path_string(path)));
        }
        Ok(text)
    }
}
pub fn parse_gitignore_line(raw: &str) -> String {
    let line = raw
        .trim_end_matches(['\n', '\r'])
        .trim_start_matches(detect_types::py_space);
    if line.is_empty() || line.starts_with('#') {
        return String::new();
    }
    static INLINE: LazyLock<regex::Regex> =
        LazyLock::new(|| crate::regex_compat::compile(r"\s+#+[^\\].*$"));
    static TRAILING: LazyLock<fancy_regex::Regex> =
        LazyLock::new(|| fancy_regex::Regex::new(r"(?<!\\) +$").unwrap());
    let line = INLINE.replace_all(line, "").replace("\\#", "#");
    paths::nfc(&TRAILING.replace_all(&line, ""))
}
pub fn find_vcs_root(start: &Path) -> Option<PathBuf> {
    let mut current = fsutil::resolve_missing(start).ok()?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    loop {
        if detect_types::strings("_VCS_MARKERS").any(|m| current.join(m).exists()) {
            return Some(current);
        }
        if home.as_ref() == Some(&current) || current.parent().is_none() {
            return None;
        }
        current = current.parent()?.to_path_buf();
    }
}
pub fn absolute_lexical(path: &Path) -> PathBuf {
    let p = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    PathBuf::from(paths::normpath(&fsutil::path_string(&p)))
}
pub fn path_identity(path: &Path) -> String {
    let absolute = fsutil::path_string(&absolute_lexical(path));
    paths::nfc(&if cfg!(windows) {
        absolute.to_lowercase().replace('/', "\\")
    } else {
        absolute
    })
}
pub fn git_tracked_path_keys(root: &Path) -> (HashSet<String>, HashSet<String>) {
    let mut files = HashSet::new();
    let mut dirs = HashSet::new();
    let Ok(root) = fsutil::resolve_missing(root) else {
        return (files, dirs);
    };
    let Some(vcs) = find_vcs_root(&root) else {
        return (files, dirs);
    };
    if !vcs.join(".git").exists() {
        return (files, dirs);
    }
    let run = process::capture(
        Command::new("git")
            .arg("-C")
            .arg(&vcs)
            .args(["ls-files", "-z", "--cached"])
            .env("GIT_OPTIONAL_LOCKS", "0"),
        Duration::from_secs(30),
    );
    let Ok(output) = run else {
        return (files, dirs);
    };
    if !output.success {
        return (files, dirs);
    }
    for raw in output.stdout.split(|b| *b == 0).filter(|b| !b.is_empty()) {
        #[cfg(unix)]
        let name = {
            use std::os::unix::ffi::OsStrExt;
            std::ffi::OsStr::from_bytes(raw).to_os_string()
        };
        #[cfg(not(unix))]
        let name = std::ffi::OsString::from(String::from_utf8_lossy(raw).into_owned());
        let path = absolute_lexical(&vcs.join(name));
        if !path.starts_with(&root) || !path.is_file() {
            continue;
        }
        files.insert(path_identity(&path));
        let mut parent = path.parent();
        while let Some(p) = parent {
            if p == root {
                break;
            }
            if !dirs.insert(path_identity(p)) {
                break;
            }
            parent = p.parent();
        }
    }
    (files, dirs)
}
pub fn git_info_exclude(root: &Path) -> Option<PathBuf> {
    let git = root.join(".git");
    let directory = if git.is_dir() {
        git
    } else if git.is_file() {
        let text = fs::read(&git)
            .ok()
            .map(|b| detect_documents::decode_ignore(&b))
            .unwrap_or_default();
        let value = text
            .trim_matches(detect_types::py_space)
            .strip_prefix("gitdir:")?
            .trim_matches(detect_types::py_space);
        let given = Path::new(value);
        let gd = if given.is_absolute() {
            given.to_path_buf()
        } else {
            fsutil::resolve_missing(&root.join(given)).ok()?
        };
        let common = gd.join("commondir");
        let text = if common.exists() {
            fs::read(common)
                .ok()
                .map(|b| detect_documents::decode_ignore(&b))
                .unwrap_or_default()
        } else {
            String::new()
        };
        let raw = text.trim_matches(detect_types::py_space);
        if raw.is_empty() {
            gd
        } else {
            let cd = Path::new(raw);
            if cd.is_absolute() {
                cd.to_path_buf()
            } else {
                fsutil::resolve_missing(&gd.join(cd)).ok()?
            }
        }
    } else {
        return None;
    };
    let exclude = directory.join("info/exclude");
    exclude.is_file().then_some(exclude)
}
pub fn load_dir_own_ignore(
    path: &Path,
    gitignore: bool,
    warnings: &mut EncodingWarnings,
) -> Result<Patterns> {
    let mut patterns = vec![];
    let names: &[&str] = if gitignore {
        &[".gitignore", ".graphicleignore"]
    } else {
        &[".graphicleignore"]
    };
    for name in names {
        let file = path.join(name);
        if file.exists() {
            for raw in fsutil::split_lines(&warnings.read(&file)?) {
                let line = parse_gitignore_line(raw);
                if !line.is_empty() {
                    patterns.push((path.to_path_buf(), line));
                }
            }
        }
    }
    Ok(patterns)
}
/// `_git_global_excludes` (0.9.86, #4335): git's user-wide excludes file
/// (`core.excludesFile`, asked with `--path` so `~` is expanded), else git's
/// default `$XDG_CONFIG_HOME/git/ignore` or `~/.config/git/ignore`. Only inside
/// a git repo, like git; `None` when there is no such file.
pub fn git_global_excludes(vcs_root: &Path) -> Option<PathBuf> {
    if !vcs_root.join(".git").exists() {
        return None;
    }
    let configured = process::capture(
        Command::new("git")
            .arg("-C")
            .arg(vcs_root)
            .args(["config", "--path", "--get", "core.excludesFile"])
            .env("GIT_OPTIONAL_LOCKS", "0"),
        Duration::from_secs(10),
    )
    .ok()
    .filter(|output| output.success)
    .map(|output| {
        String::from_utf8_lossy(&output.stdout)
            .trim_matches(detect_types::py_space)
            .to_owned()
    })
    .unwrap_or_default();
    let home = || {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
    };
    let path = if !configured.is_empty() {
        let path = if configured == "~" {
            home()
        } else if let Some(rest) = configured.strip_prefix("~/") {
            home().join(rest)
        } else {
            PathBuf::from(&configured)
        };
        if path.is_absolute() {
            path
        } else {
            vcs_root.join(path)
        }
    } else {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|xdg| !xdg.is_empty() && Path::new(xdg).is_absolute())
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".config"));
        base.join("git").join("ignore")
    };
    path.is_file().then_some(path)
}
pub fn load_ignore_file(
    root: &Path,
    gitignore: bool,
    warnings: &mut EncodingWarnings,
) -> Result<Patterns> {
    let root = fsutil::resolve_missing(root).map_err(crate::Error::from)?;
    let ceiling = find_vcs_root(&root).unwrap_or_else(|| root.clone());
    let mut dirs = vec![];
    let mut current = root;
    loop {
        dirs.push(current.clone());
        if current == ceiling {
            break;
        }
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent.into();
    }
    dirs.reverse();
    let mut patterns = vec![];
    // git's user-wide excludes rank below info/exclude: first of all,
    // anchored at the VCS root like info/exclude (0.9.86, #4335).
    if let Some(global) = gitignore.then(|| git_global_excludes(&ceiling)).flatten() {
        for raw in fsutil::split_lines(&warnings.read(&global)?) {
            let line = parse_gitignore_line(raw);
            if !line.is_empty() {
                patterns.push((ceiling.clone(), line));
            }
        }
    }
    if let Some(exclude) = gitignore.then(|| git_info_exclude(&ceiling)).flatten() {
        for raw in fsutil::split_lines(&warnings.read(&exclude)?) {
            let line = parse_gitignore_line(raw);
            if !line.is_empty() {
                patterns.push((ceiling.clone(), line));
            }
        }
    }
    for path in dirs {
        patterns.extend(load_dir_own_ignore(&path, gitignore, warnings)?);
    }
    Ok(patterns)
}
#[derive(Clone, serde::Serialize)]
pub struct ParsedPattern(pub bool, pub bool, pub bool, pub String);
static PATTERNS: LazyLock<Mutex<HashMap<String, ParsedPattern>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// [`parse_ignore_pattern`]'s fields, borrowed from `pattern`: parsing is a few
/// string checks, cheaper than the shared cache (whose lock every matching
/// thread would take for every pattern of every path).
fn parse_pattern_ref(pattern: &str) -> (bool, bool, bool, &str) {
    let negated = pattern.starts_with('!');
    let raw = pattern.strip_prefix('!').unwrap_or(pattern);
    (
        negated,
        raw.ends_with('/'),
        raw.trim_end_matches('/').contains('/'),
        raw.trim_matches('/'),
    )
}
pub fn parse_ignore_pattern(pattern: &str) -> ParsedPattern {
    let mut cache = PATTERNS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(got) = cache.get(pattern) {
        return got.clone();
    }
    let negated = pattern.starts_with('!');
    let raw = pattern.strip_prefix('!').unwrap_or(pattern);
    let value = ParsedPattern(
        negated,
        raw.ends_with('/'),
        raw.trim_end_matches('/').contains('/'),
        raw.trim_matches('/').into(),
    );
    if cache.len() >= 100000 {
        cache.clear();
    }
    cache.insert(pattern.into(), value.clone());
    value
}
pub fn lexical_relative(target: &Path, anchor: &Path) -> Option<String> {
    let target = path_syntax::posix_parts(&fsutil::path_string(target));
    let anchor = path_syntax::posix_parts(&fsutil::path_string(anchor));
    if target.len() < anchor.len()
        || !target
            .iter()
            .zip(&anchor)
            .all(|(a, b)| a == b || (cfg!(windows) && a.to_lowercase() == b.to_lowercase()))
    {
        return None;
    }
    Some(paths::nfc(&if target.len() == anchor.len() {
        ".".into()
    } else {
        target[anchor.len()..].join("/")
    }))
}
/// fnmatchcase-style wildcard matching. Unlike filesystem glob, '*' may cross '/'.
pub fn wildcard(text: &str, pattern: &str) -> bool {
    // `*` is the only variable-length token, so greedy matching that backtracks
    // to the last `*` decides the same language as the memoized recursion it
    // replaces (kept as `wildcard_reference` in the tests).
    let t: smallvec::SmallVec<[char; 64]> = text.chars().collect();
    let p: smallvec::SmallVec<[char; 32]> = pattern.chars().collect();
    let (mut i, mut j) = (0, 0);
    let mut resume: Option<(usize, usize)> = None;
    while i < t.len() {
        if j < p.len() {
            let step = match p[j] {
                '*' => {
                    resume = Some((j + 1, i));
                    j += 1;
                    continue;
                }
                '?' => Some(j + 1),
                '[' => class_step(&p, j, t[i]),
                c => (c == t[i]).then_some(j + 1),
            };
            if let Some(next) = step {
                i += 1;
                j = next;
                continue;
            }
        }
        match resume {
            Some((after, start)) => {
                j = after;
                i = start + 1;
                resume = Some((after, start + 1));
            }
            None => return false,
        }
    }
    p[j..].iter().all(|c| *c == '*')
}
/// The pattern index after a `[...]` class at `j` that matches `c`, or `None`.
/// An unclosed `[` is a literal bracket.
fn class_step(p: &[char], j: usize, c: char) -> Option<usize> {
    let mut end = j + 1;
    if p.get(end) == Some(&'!') {
        end += 1;
    }
    if p.get(end) == Some(&']') {
        end += 1;
    }
    while end < p.len() && p[end] != ']' {
        end += 1;
    }
    if end == p.len() {
        return (c == '[').then_some(j + 1);
    }
    let mut start = j + 1;
    let neg = p.get(start) == Some(&'!');
    if neg {
        start += 1;
    }
    let mut hit = false;
    let mut n = start;
    while n < end {
        if n + 2 < end && p[n + 1] == '-' {
            hit |= p[n] <= c && c <= p[n + 2];
            n += 3;
        } else {
            hit |= p[n] == c;
            n += 1;
        }
    }
    (hit != neg).then_some(end + 1)
}
#[cfg(test)]
fn wildcard_reference(text: &str, pattern: &str) -> bool {
    let text: Vec<char> = text.chars().collect();
    let pattern: Vec<char> = pattern.chars().collect();
    fn go(
        t: &[char],
        p: &[char],
        i: usize,
        j: usize,
        memo: &mut HashMap<(usize, usize), bool>,
    ) -> bool {
        if let Some(v) = memo.get(&(i, j)) {
            return *v;
        }
        let value = if j == p.len() {
            i == t.len()
        } else {
            match p[j] {
                '*' => go(t, p, i, j + 1, memo) || (i < t.len() && go(t, p, i + 1, j, memo)),
                '?' => i < t.len() && go(t, p, i + 1, j + 1, memo),
                '[' => {
                    let mut end = j + 1;
                    if p.get(end) == Some(&'!') {
                        end += 1;
                    }
                    if p.get(end) == Some(&']') {
                        end += 1;
                    }
                    while end < p.len() && p[end] != ']' {
                        end += 1;
                    }
                    if end == p.len() {
                        i < t.len() && t[i] == '[' && go(t, p, i + 1, j + 1, memo)
                    } else if i == t.len() {
                        false
                    } else {
                        let mut start = j + 1;
                        let neg = p.get(start) == Some(&'!');
                        if neg {
                            start += 1;
                        }
                        let mut hit = false;
                        let mut n = start;
                        while n < end {
                            if n + 2 < end && p[n + 1] == '-' {
                                hit |= p[n] <= t[i] && t[i] <= p[n + 2];
                                n += 3;
                            } else {
                                hit |= p[n] == t[i];
                                n += 1;
                            }
                        }
                        (hit != neg) && go(t, p, i + 1, end + 1, memo)
                    }
                }
                c => i < t.len() && c == t[i] && go(t, p, i + 1, j + 1, memo),
            }
        };
        memo.insert((i, j), value);
        value
    }
    go(&text, &pattern, 0, 0, &mut HashMap::new())
}
pub fn match_globstar_parts(
    path: &[String],
    pattern: &[String],
    i: usize,
    j: usize,
    memo: &mut HashMap<(usize, usize), bool>,
) -> bool {
    if let Some(v) = memo.get(&(i, j)) {
        return *v;
    }
    let value = if j == pattern.len() {
        i == path.len()
    } else if pattern[j] == "**" {
        if j == pattern.len() - 1 {
            i < path.len()
        } else {
            match_globstar_parts(path, pattern, i, j + 1, memo)
                || (i < path.len() && match_globstar_parts(path, pattern, i + 1, j, memo))
        }
    } else {
        i < path.len()
            && wildcard(&path[i], &pattern[j])
            && match_globstar_parts(path, pattern, i + 1, j + 1, memo)
    };
    memo.insert((i, j), value);
    value
}
pub fn match_anchored_ignore_pattern(path: &str, pattern: &str) -> bool {
    let path = path.split('/').map(str::to_owned).collect::<Vec<_>>();
    let pattern = pattern.split('/').map(str::to_owned).collect::<Vec<_>>();
    if !pattern.iter().any(|p| p == "**") {
        return path.len() == pattern.len()
            && path.iter().zip(&pattern).all(|(a, b)| wildcard(a, b));
    }
    match_globstar_parts(&path, &pattern, 0, 0, &mut HashMap::new())
}
/// A pattern without a slash names one path component, so it is compared
/// with the target's own name only (0.9.85, #4242). Ancestors are tested one
/// by one by [`is_ignored`]'s parent walk; matching them here again let `*`
/// cross `/` and left a directory re-included with `!dir/` without its files.
fn unanchored_matches(name: &str, pattern: &str) -> bool {
    if cfg!(windows) {
        wildcard(&name.to_lowercase(), &pattern.to_lowercase())
    } else {
        wildcard(name, pattern)
    }
}
fn evaluate(target: &Path, patterns: &Patterns, cache: &mut MatchCache) -> bool {
    if let Some(v) = cache.get(target) {
        return *v;
    }
    let mut result = false;
    let mut per_anchor: HashMap<&Path, Option<String>> = HashMap::new();
    let mut target_dir = None;
    // Per-target values the pattern loop would otherwise recompute per pattern.
    let name = paths::nfc(&target.file_name().unwrap_or_default().to_string_lossy());
    for (anchor, pattern) in patterns {
        // A pattern only applies below its anchor. Most anchors of a large
        // tree (one per ignore file) are not ancestors of the target: skip
        // them before any string work. `Path::starts_with` compares whole
        // components like `lexical_relative`, which decides the rest.
        if !cfg!(windows)
            && target.is_absolute()
            && anchor.is_absolute()
            && !target.starts_with(anchor)
        {
            continue;
        }
        let rel = per_anchor
            .entry(anchor)
            .or_insert_with(|| lexical_relative(target, anchor));
        let Some(rel) = rel else {
            continue;
        };
        if rel == "." {
            continue;
        }
        let (negated, directory_only, path_relative, p) = parse_pattern_ref(pattern);
        if p.is_empty() {
            continue;
        }
        let mut matched = if path_relative {
            match_anchored_ignore_pattern(rel, p)
        } else {
            unanchored_matches(&name, p)
        };
        if matched && directory_only {
            matched = *target_dir.get_or_insert_with(|| target.is_dir());
        }
        if matched {
            result = !negated;
        }
    }
    cache.insert(target.to_path_buf(), result);
    result
}
pub fn is_ignored(path: &Path, root: &Path, patterns: &Patterns, cache: &mut MatchCache) -> bool {
    if patterns.is_empty() {
        return false;
    }
    if let Ok(relative) = path.strip_prefix(root) {
        let parts = relative.components().collect::<Vec<_>>();
        let mut ancestor = root.to_path_buf();
        for part in parts.iter().take(parts.len().saturating_sub(1)) {
            ancestor.push(part.as_os_str());
            if evaluate(&ancestor, patterns, cache) {
                return true;
            }
        }
    }
    evaluate(path, patterns, cache)
}
pub struct IgnoreState {
    pub root: PathBuf,
    pub patterns: Patterns,
    pub explicit: Patterns,
    pub tracked_files: HashSet<String>,
    pub tracked_dirs: HashSet<String>,
    pub cache: MatchCache,
    pub explicit_cache: MatchCache,
    pub loaded: HashSet<PathBuf>,
    pub gitignore: bool,
}
impl IgnoreState {
    pub fn new(
        root: &Path,
        extra: &[String],
        gitignore: bool,
        warnings: &mut EncodingWarnings,
    ) -> Result<Self> {
        let root = fsutil::resolve_missing(root).map_err(crate::Error::from)?;
        let mut patterns = load_ignore_file(&root, gitignore, warnings)?;
        let mut explicit = load_ignore_file(&root, false, warnings)?;
        let (tracked_files, tracked_dirs) = if gitignore && patterns.len() > explicit.len() {
            git_tracked_path_keys(&root)
        } else {
            (HashSet::new(), HashSet::new())
        };
        for raw in extra {
            let line = parse_gitignore_line(raw);
            if !line.is_empty() {
                patterns.push((root.clone(), line.clone()));
                explicit.push((root.clone(), line));
            }
        }
        Ok(Self {
            loaded: HashSet::from([root.clone()]),
            root,
            patterns,
            explicit,
            tracked_files,
            tracked_dirs,
            cache: HashMap::new(),
            explicit_cache: HashMap::new(),
            gitignore,
        })
    }
    pub fn load_directory(&mut self, path: &Path, warnings: &mut EncodingWarnings) -> Result<()> {
        if self.loaded.insert(path.into()) {
            self.patterns
                .extend(load_dir_own_ignore(path, self.gitignore, warnings)?);
            self.explicit
                .extend(load_dir_own_ignore(path, false, warnings)?);
        }
        Ok(())
    }
    pub fn is_scan_ignored(&mut self, path: &Path) -> bool {
        if !is_ignored(path, &self.root, &self.patterns, &mut self.cache) {
            return false;
        }
        if is_ignored(path, &self.root, &self.explicit, &mut self.explicit_cache) {
            return true;
        }
        let key = path_identity(path);
        !self.tracked_files.contains(&key) && !self.tracked_dirs.contains(&key)
    }
    /// [`Self::is_scan_ignored`] with caller-owned match caches (pattern and
    /// explicit-pattern memo), so threads can share the rules: the caches
    /// only memoise the match, never change it.
    pub fn is_scan_ignored_with(&self, path: &Path, caches: &mut (MatchCache, MatchCache)) -> bool {
        if !is_ignored(path, &self.root, &self.patterns, &mut caches.0) {
            return false;
        }
        if is_ignored(path, &self.root, &self.explicit, &mut caches.1) {
            return true;
        }
        let key = path_identity(path);
        !self.tracked_files.contains(&key) && !self.tracked_dirs.contains(&key)
    }
    pub fn predicate(&mut self, path: &Path, warnings: &mut EncodingWarnings) -> Result<bool> {
        let path = absolute_lexical(path);
        let Ok(rel) = path.strip_prefix(&self.root) else {
            return Ok(false);
        };
        let parts = rel.components().collect::<Vec<_>>();
        if detect_types::contains(
            "_SKIP_FILES",
            &path.file_name().unwrap_or_default().to_string_lossy(),
        ) || detect_types::is_installed_rule_file(&path)
        {
            return Ok(true);
        }
        let mut parent = self.root.clone();
        for part in parts.iter().take(parts.len().saturating_sub(1)) {
            if detect_types::is_noise_dir(&part.as_os_str().to_string_lossy(), Some(&parent)) {
                return Ok(true);
            }
            parent.push(part.as_os_str());
        }
        let mut ancestor = self.root.clone();
        for part in parts.iter().take(parts.len().saturating_sub(1)) {
            ancestor.push(part.as_os_str());
            self.load_directory(&ancestor, warnings)?;
        }
        Ok(self.is_scan_ignored(&path))
    }
}
pub fn auto_follow_symlinks(root: &Path) -> bool {
    fs::read_dir(root).is_ok_and(|entries| entries.flatten().any(|e| e.path().is_symlink()))
}
pub fn resolves_under_root(path: &Path, root: &Path) -> bool {
    match (fsutil::resolve_missing(path), fsutil::resolve_missing(root)) {
        (Ok(path), Ok(root)) => path.starts_with(root),
        _ => false,
    }
}
/// [`resolves_under_root`] with the root already resolved (`None`: it could
/// not be) and each directory resolved once through `dirs`. Resolving a
/// directory and then a name from it walks the same components as resolving
/// the whole path, one `lstat` per component.
pub fn resolves_under_resolved_root(
    path: &Path,
    root: Option<&Path>,
    dirs: &mut HashMap<PathBuf, PathBuf>,
) -> bool {
    let Some(root) = root else {
        return false;
    };
    let resolved = match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) if path.is_absolute() => {
            let dir = match dirs.get(parent) {
                Some(dir) => Ok(dir.clone()),
                None => fsutil::resolve_missing(parent).inspect(|dir| {
                    dirs.insert(parent.to_path_buf(), dir.clone());
                }),
            };
            dir.and_then(|dir| fsutil::resolve_missing_from(&dir, Path::new(name)))
        }
        _ => fsutil::resolve_missing(path),
    };
    resolved.is_ok_and(|p| p.starts_with(root))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn siblings_reuse_ancestor_cache_entries() {
        let root = Path::new("/repo");
        let patterns = vec![(root.into(), "*.tmp".into())];
        let mut cache = MatchCache::new();
        for name in ["a/b/first.py", "a/b/second.py", "a/c/third.py"] {
            assert!(!is_ignored(&root.join(name), root, &patterns, &mut cache));
        }
        assert_eq!(cache.len(), 6);
        let original = cache.clone();
        assert!(!is_ignored(
            &root.join("a/b/first.py"),
            root,
            &patterns,
            &mut cache
        ));
        assert_eq!(cache, original);
    }
}

#[cfg(test)]
mod wildcard_tests {
    use super::{wildcard, wildcard_reference};
    use rand::{Rng, SeedableRng};
    use rand_xorshift::XorShiftRng;

    #[test]
    fn greedy_matcher_agrees_with_the_memoized_reference() {
        let mut rng = XorShiftRng::seed_from_u64(1206);
        let text_chars = ['a', 'b', '.', '/', '[', ']', '-', 'é', '!', '*'];
        let pattern_chars = ['a', 'b', '.', '/', '*', '*', '?', '[', ']', '!', '-', 'é'];
        for _ in 0..60_000 {
            let t: String = (0..rng.gen_range(0..9))
                .map(|_| text_chars[rng.gen_range(0..text_chars.len())])
                .collect();
            let p: String = (0..rng.gen_range(0..8))
                .map(|_| pattern_chars[rng.gen_range(0..pattern_chars.len())])
                .collect();
            assert_eq!(
                wildcard(&t, &p),
                wildcard_reference(&t, &p),
                "text={t:?} pattern={p:?}"
            );
        }
        for (t, p) in [
            ("node_modules", "node_*"),
            ("foo.pyc", "*.py[co]"),
            ("a-b", "[a-c]-[!x]"),
            ("[x", "[x"),
            ("", "*"),
            ("", ""),
            ("abc", "a*b*c*"),
            ("mississippi", "m*iss*ppi"),
        ] {
            assert_eq!(wildcard(t, p), wildcard_reference(t, p), "{t:?} {p:?}");
        }
    }
}
