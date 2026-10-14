// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/rcfile.py
//! Project-level configuration: `<root>/.graphiclerc` (upstream `.graphifyrc`).
//!
//! One `key=value` per line, `#` comments, blank lines ignored. Keys:
//!
//! - `viz_node_limit=<int>`: baked into the generated git hooks.
//! - `language.<ext>=<language | .ext>`: treat files with extension `<ext>` as
//!   the named language for classification, extractor dispatch and cross-file
//!   resolution (#2961). `<ext>` may be written with or without its leading
//!   dot; the value is a name from [`LANGUAGE_ALIASES`] or an extension the
//!   engine already knows (`.php`, `.pas`, `.sql`, ...). An ambiguous
//!   extension (`.inc` is PHP, Pascal, SQL or assembly depending on the
//!   project) has no single correct global mapping, so the project declares it.
//!
//! Active overrides are per thread, as upstream keeps them: two scans of
//! different roots in one process never see each other's declarations. The
//! reference hands its overrides to extraction workers through the pool
//! initializer; here [`crate::par`] installs the caller's [`snapshot`] in
//! every worker it starts, and a thread that never set overrides of its own
//! (any other worker thread) reads the ones most recently set in the process.
use crate::{Result, dedup_strings::whitespace, detect_types, fsutil, io_compat, path_syntax};
use serde_json::{Map, Value, json};
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashSet},
    path::Path,
    sync::{Arc, LazyLock, Mutex, RwLock},
};

pub const RC_FILENAME: &str = ".graphiclerc";

/// Language name -> the canonical extension the engine dispatches it under.
/// Deliberately a short list of names a user is likely to type; any known
/// extension can be given directly instead (`language.inc=.php`).
pub const LANGUAGE_ALIASES: &[(&str, &str)] = &[
    ("php", ".php"),
    ("pascal", ".pas"),
    ("delphi", ".pas"),
    ("freepascal", ".pas"),
    ("sql", ".sql"),
    ("python", ".py"),
    ("javascript", ".js"),
    ("js", ".js"),
    ("typescript", ".ts"),
    ("ts", ".ts"),
    ("c", ".c"),
    ("cpp", ".cpp"),
    ("c++", ".cpp"),
    ("cxx", ".cpp"),
    ("objc", ".mm"),
    ("objective-c", ".mm"),
    ("objectivec", ".mm"),
    ("java", ".java"),
    ("kotlin", ".kt"),
    ("scala", ".scala"),
    ("groovy", ".groovy"),
    ("go", ".go"),
    ("golang", ".go"),
    ("rust", ".rs"),
    ("ruby", ".rb"),
    ("csharp", ".cs"),
    ("c#", ".cs"),
    ("swift", ".swift"),
    ("lua", ".lua"),
    ("zig", ".zig"),
    ("elixir", ".ex"),
    ("julia", ".jl"),
    ("dart", ".dart"),
    ("r", ".r"),
    ("fortran", ".f90"),
    ("shell", ".sh"),
    ("bash", ".sh"),
    ("sh", ".sh"),
    ("powershell", ".ps1"),
    ("verilog", ".v"),
    ("systemverilog", ".sv"),
    ("terraform", ".tf"),
    ("hcl", ".hcl"),
    ("ocaml", ".ml"),
    ("lisp", ".lisp"),
    ("commonlisp", ".lisp"),
    ("markdown", ".md"),
    ("json", ".json"),
    ("yaml", ".yaml"),
    ("html", ".html"),
];

/// Extension -> declared canonical extension, both lower-cased with a dot.
pub type LanguageOverrides = BTreeMap<String, String>;

/// The parsed file. `languages` keeps the file's order (a repeated extension
/// keeps its first position and takes its last value, like a Python dict).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RcConfig {
    pub viz_node_limit: Option<i64>,
    pub languages: Vec<(String, String)>,
}

impl RcConfig {
    pub fn languages(&self) -> LanguageOverrides {
        self.languages.iter().cloned().collect()
    }

    /// The reference's dict: each key present only when the file sets it.
    pub fn to_json(&self) -> Value {
        let mut out = Map::new();
        if let Some(limit) = self.viz_node_limit {
            out.insert("viz_node_limit".into(), json!(limit));
        }
        if !self.languages.is_empty() {
            let languages: Map<String, Value> = self
                .languages
                .iter()
                .map(|(ext, target)| (ext.clone(), json!(target)))
                .collect();
            out.insert("languages".into(), Value::Object(languages));
        }
        Value::Object(out)
    }
}

fn py_strip(value: &str) -> &str {
    value.trim_matches(whitespace)
}

/// `_normalise_ext`: stripped, lower-cased, with a leading dot.
pub fn normalise_ext(raw: &str) -> String {
    let ext = py_strip(raw).to_lowercase();
    if ext.starts_with('.') {
        ext
    } else {
        format!(".{ext}")
    }
}

/// Resolve the right-hand side of `language.<ext>=` to a canonical suffix: a
/// name from [`LANGUAGE_ALIASES`] (case-insensitive) or an explicit dotted
/// extension the engine classifies. Anything else is an error.
pub fn parse_language_value(value: &str) -> Result<String> {
    let v = py_strip(value);
    if v.is_empty() {
        return Err(crate::Error::invalid("empty value"));
    }
    let shown = crate::python_compat::repr_string(value);
    if v.starts_with('.') {
        let ext = normalise_ext(v);
        if ext.chars().count() < 2 || ext.chars().any(whitespace) {
            return Err(crate::Error::invalid(format!(
                "{shown} is not an extension"
            )));
        }
        if !detect_types::contains("CODE_EXTENSIONS", &ext)
            && !detect_types::contains("DOC_EXTENSIONS", &ext)
        {
            return Err(crate::Error::invalid(format!("unknown extension {shown}")));
        }
        return Ok(ext);
    }
    let wanted = v.to_lowercase();
    if let Some((_, target)) = LANGUAGE_ALIASES.iter().find(|(name, _)| *name == wanted) {
        return Ok((*target).into());
    }
    let mut known = LANGUAGE_ALIASES
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>();
    known.sort_unstable();
    Err(crate::Error::invalid(format!(
        "unknown language {shown} (use one of: {}; or an explicit extension such as .php)",
        known.join(", ")
    )))
}

/// Python's `int()` on a stripped string: optional sign, ASCII or Unicode
/// decimal digits, single underscores between digits.
fn python_int(text: &str) -> Option<i64> {
    let (negative, digits) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    if digits.is_empty() || digits.starts_with('_') || digits.ends_with('_') {
        return None;
    }
    let mut value: i64 = 0;
    let mut previous_underscore = false;
    for c in digits.chars() {
        if c == '_' {
            if previous_underscore {
                return None;
            }
            previous_underscore = true;
            continue;
        }
        previous_underscore = false;
        let digit = c.to_digit(10).or_else(|| {
            let ranges = crate::decimal_data::DECIMAL;
            let i = ranges.partition_point(|&(lo, _)| lo <= c as u32);
            (i > 0 && c as u32 <= ranges[i - 1].1).then(|| (c as u32 - ranges[i - 1].0) % 10)
        })?;
        value = value.checked_mul(10)?.checked_add(i64::from(digit))?;
    }
    Some(if negative { -value } else { value })
}

fn utf8_error(bytes: &[u8], error: std::str::Utf8Error) -> String {
    let pos = error.valid_up_to();
    let len = error.error_len().unwrap_or(bytes.len() - pos);
    let place = if len == 1 {
        format!("byte 0x{:02x} in position {pos}", bytes[pos])
    } else {
        format!("bytes in position {pos}-{}", pos + len - 1)
    };
    let reason = if error.error_len().is_none() {
        "unexpected end of data"
    } else if !(0xc2..=0xf4).contains(&bytes[pos]) {
        "invalid start byte"
    } else {
        "invalid continuation byte"
    };
    format!("'utf-8' codec can't decode {place}: {reason}")
}

/// Parse `<root>/.graphiclerc`; an absent file is an empty config. Unknown
/// keys are ignored so a newer release's options do not break an older one.
/// A malformed line is an error naming the line.
pub fn load_rc_file(root: &Path) -> Result<RcConfig> {
    let rc_path = root.join(RC_FILENAME);
    if !rc_path.is_file() {
        return Ok(RcConfig::default());
    }
    let shown_path = fsutil::path_string(&rc_path);
    let bytes = std::fs::read(&rc_path)
        .map_err(|error| crate::Error::External(io_compat::read_error(&error, &rc_path)))?;
    let content = std::str::from_utf8(&bytes)
        .map_err(|error| crate::Error::invalid(utf8_error(&bytes, error)))?;
    let mut config = RcConfig::default();
    for (index, raw) in fsutil::split_lines(content).enumerate() {
        let line_num = index + 1;
        let line = py_strip(raw);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, val)) = line.split_once('=') else {
            return Err(crate::Error::invalid(format!(
                "Invalid line {line_num} in {shown_path}: {} (expected key=value)",
                crate::python_compat::repr_string(raw)
            )));
        };
        let key = py_strip(key);
        let val = py_strip(val);
        if key == "viz_node_limit" {
            let parsed = python_int(val).filter(|n| *n >= 0).ok_or_else(|| {
                crate::Error::invalid(format!(
                    "Invalid viz_node_limit in {shown_path} at line {line_num}: {}. Must be a non-negative integer.",
                    crate::python_compat::repr_string(val)
                ))
            })?;
            config.viz_node_limit = Some(parsed);
        } else if let Some(ext_part) = key.strip_prefix("language.") {
            let ext_part = py_strip(ext_part);
            if ext_part.is_empty() || ext_part.chars().any(whitespace) {
                return Err(crate::Error::invalid(format!(
                    "Invalid language key in {shown_path} at line {line_num}: {} (expected language.<ext>=<language>)",
                    crate::python_compat::repr_string(key)
                )));
            }
            let target = parse_language_value(val).map_err(|error| {
                crate::Error::invalid(format!(
                    "Invalid {key} in {shown_path} at line {line_num}: {error}"
                ))
            })?;
            let ext = normalise_ext(ext_part);
            match config.languages.iter_mut().find(|(known, _)| *known == ext) {
                Some(slot) => slot.1 = target,
                None => config.languages.push((ext, target)),
            }
        }
    }
    Ok(config)
}

// ---------------------------------------------------------------------------
// Active overrides
// ---------------------------------------------------------------------------

thread_local! {
    /// This thread's overrides, once it set (or was handed) its own.
    static LOCAL: RefCell<Option<Arc<LanguageOverrides>>> = const { RefCell::new(None) };
    /// Warnings collected by [`capture_warnings`] instead of printed.
    static CAPTURED: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}
/// The overrides most recently set in the process: what a thread that never
/// set its own sees (an extraction worker started outside [`crate::par`]).
static PROCESS: LazyLock<RwLock<Arc<LanguageOverrides>>> = LazyLock::new(Default::default);
/// Roots whose malformed file was already reported (warning dedup only).
static WARNED_ROOTS: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);

fn active() -> Arc<LanguageOverrides> {
    LOCAL
        .with(|local| local.borrow().clone())
        .unwrap_or_else(|| PROCESS.read().map(|p| Arc::clone(&p)).unwrap_or_default())
}

/// The overrides the current thread sees, to hand to a worker thread.
#[derive(Clone, Debug, Default)]
pub struct Snapshot(Arc<LanguageOverrides>);

/// What this thread sees now; [`Snapshot::install`] gives a worker the same.
pub fn snapshot() -> Snapshot {
    Snapshot(active())
}

impl Snapshot {
    /// Make these the calling thread's overrides (the reference's pool
    /// initializer). The process-wide fallback is left as it is.
    pub fn install(&self) {
        LOCAL.with(|local| *local.borrow_mut() = Some(Arc::clone(&self.0)));
    }

    pub fn overrides(&self) -> &LanguageOverrides {
        &self.0
    }
}

/// Replace this thread's extension overrides (`{".inc": ".php"}`); both sides
/// are normalised. `None` clears them.
pub fn set_language_overrides(mapping: Option<&LanguageOverrides>) {
    let normalised: LanguageOverrides = mapping
        .into_iter()
        .flatten()
        .map(|(ext, target)| (normalise_ext(ext), normalise_ext(target)))
        .collect();
    let normalised = Arc::new(normalised);
    LOCAL.with(|local| *local.borrow_mut() = Some(Arc::clone(&normalised)));
    if let Ok(mut process) = PROCESS.write() {
        *process = normalised;
    }
}

pub fn get_language_overrides() -> LanguageOverrides {
    (*active()).clone()
}

fn warn(message: String) {
    let unclaimed = CAPTURED.with(|captured| match captured.borrow_mut().as_mut() {
        Some(buffer) => {
            buffer.push(message);
            None
        }
        None => Some(message),
    });
    if let Some(message) = unclaimed {
        eprintln!("{message}");
    }
}

/// Run `f` with this thread's rc-file warnings collected instead of printed
/// (JSON callers report them in their result).
pub fn capture_warnings<R>(f: impl FnOnce() -> R) -> (R, Vec<String>) {
    let previous = CAPTURED.with(|captured| captured.borrow_mut().replace(Vec::new()));
    let result = f();
    let warnings = CAPTURED.with(|captured| {
        std::mem::replace(&mut *captured.borrow_mut(), previous).unwrap_or_default()
    });
    (result, warnings)
}

/// Forget which roots were warned about (a fresh process's state).
pub fn clear_warned_roots() {
    if let Ok(mut warned) = WARNED_ROOTS.lock() {
        warned.clear();
    }
}

/// Load `<root>/.graphiclerc` and make its language overrides active. A
/// malformed file is reported once per root and treated as having no
/// overrides: a scan must not die on a config typo, but the user must hear
/// about it. Returns the mapping now active.
pub fn activate_language_overrides(root: &Path) -> LanguageOverrides {
    let config = match load_rc_file(root) {
        Ok(config) => config,
        Err(error) => {
            let key = fsutil::path_string(root);
            let first = WARNED_ROOTS
                .lock()
                .map(|mut warned| warned.insert(key))
                .unwrap_or(false);
            if first {
                warn(format!(
                    "[graphicle] warning: ignoring {RC_FILENAME}: {error}"
                ));
            }
            RcConfig::default()
        }
    };
    set_language_overrides(Some(&config.languages()));
    get_language_overrides()
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The suffix `path` is treated as having: the declared one when its
/// extension is remapped, else the real suffix untouched (case preserved, so
/// callers that tell `.F90` from `.f90` keep doing so).
pub fn effective_suffix(path: impl AsRef<Path>) -> String {
    let name = file_name(path.as_ref());
    let suffix = path_syntax::suffix(&name);
    let active = active();
    if active.is_empty() {
        return suffix.to_owned();
    }
    active
        .get(&suffix.to_lowercase())
        .cloned()
        .unwrap_or_else(|| suffix.to_owned())
}

/// Extra cache-key material for a remapped file, else `None`: the same bytes
/// parse to a different graph under a different extractor, so a `.inc`
/// cached as Pascal must not be served once the project declares it PHP.
pub fn cache_salt(path: impl AsRef<Path>) -> Option<String> {
    let active = active();
    if active.is_empty() {
        return None;
    }
    let name = file_name(path.as_ref());
    active
        .get(&path_syntax::suffix(&name).to_lowercase())
        .map(|target| format!("language={target}"))
}

fn overrides_json(overrides: &LanguageOverrides) -> Value {
    json!(overrides)
}

/// Development JSON bindings (`{"operation": "rcfile", "function": ...}`).
pub fn dispatch(r: &Value) -> Result<Value> {
    let s = |key: &str| r[key].as_str().unwrap_or("");
    let path = || Path::new(s("path"));
    let (result, warnings) = capture_warnings(|| -> Result<Value> {
        match s("function") {
            "constants" => Ok(json!({
                "RC_FILENAME": RC_FILENAME,
                "LANGUAGE_ALIASES": LANGUAGE_ALIASES
                    .iter()
                    .map(|(name, ext)| ((*name).to_owned(), json!(ext)))
                    .collect::<Map<_, _>>(),
            })),
            "_normalise_ext" => Ok(json!(normalise_ext(s("raw")))),
            "parse_language_value" => parse_language_value(s("value")).map(|v| json!(v)),
            "load_rc_file" => load_rc_file(Path::new(s("root"))).map(|c| c.to_json()),
            "set_language_overrides" => {
                let mapping = r["mapping"].as_object().map(|object| {
                    object
                        .iter()
                        .map(|(ext, target)| {
                            (ext.clone(), target.as_str().unwrap_or_default().to_owned())
                        })
                        .collect::<LanguageOverrides>()
                });
                set_language_overrides(mapping.as_ref());
                Ok(Value::Null)
            }
            "get_language_overrides" => Ok(overrides_json(&get_language_overrides())),
            "activate_language_overrides" => Ok(overrides_json(&activate_language_overrides(
                Path::new(s("root")),
            ))),
            "effective_suffix" => Ok(json!(effective_suffix(path()))),
            "cache_salt" => Ok(json!(cache_salt(path()))),
            "_warned_roots.clear" => {
                clear_warned_roots();
                Ok(Value::Null)
            }
            other => Err(crate::Error::invalid(format!(
                "Unknown rcfile function: {other}"
            ))),
        }
    });
    result.map(|value| json!({"value": value, "warnings": warnings}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn reset() {
        set_language_overrides(None);
        clear_warned_roots();
    }

    #[test]
    fn a_language_line_maps_an_extension_to_a_canonical_suffix() {
        reset();
        let dir = tempfile::tempdir().unwrap();
        for key in [
            "language.inc",
            "language..inc",
            "language.INC",
            "language. inc ",
        ] {
            fs::write(dir.path().join(RC_FILENAME), format!("{key}=php\n")).unwrap();
            let config = load_rc_file(dir.path()).unwrap();
            assert_eq!(
                config.languages,
                vec![(".inc".into(), ".php".into())],
                "{key}"
            );
            assert_eq!(config.to_json(), json!({"languages": {".inc": ".php"}}));
        }
    }

    #[test]
    fn values_accept_a_name_or_a_known_extension() {
        for (value, expected) in [
            ("php", ".php"),
            ("PHP", ".php"),
            ("pascal", ".pas"),
            ("delphi", ".pas"),
            ("typescript", ".ts"),
            ("c++", ".cpp"),
            (".php", ".php"),
            (".PHP", ".php"),
            ("markdown", ".md"),
            (".md", ".md"),
        ] {
            assert_eq!(parse_language_value(value).unwrap(), expected, "{value}");
        }
        for value in ["", "klingon", ".", ". php", "php script"] {
            assert!(parse_language_value(value).is_err(), "{value}");
        }
        let error = parse_language_value("klingon").unwrap_err().to_string();
        assert!(error.starts_with("unknown language 'klingon' (use one of: bash, c, c#, c++"));
        assert_eq!(
            parse_language_value(".phhp").unwrap_err().to_string(),
            "unknown extension '.phhp'"
        );
    }

    #[test]
    fn malformed_lines_name_their_line() {
        let dir = tempfile::tempdir().unwrap();
        let rc = dir.path().join(RC_FILENAME);
        fs::write(&rc, "viz_node_limit=5\nlanguage.inc=klingon\n").unwrap();
        let error = load_rc_file(dir.path()).unwrap_err().to_string();
        assert!(error.starts_with("Invalid language.inc in "), "{error}");
        assert!(
            error.contains(" at line 2: unknown language 'klingon'"),
            "{error}"
        );
        fs::write(&rc, "language.=php\n").unwrap();
        let error = load_rc_file(dir.path()).unwrap_err().to_string();
        assert!(error.contains("at line 1: 'language.' (expected language.<ext>=<language>)"));
        fs::write(&rc, "viz_node_limit=-1\n").unwrap();
        assert!(
            load_rc_file(dir.path())
                .unwrap_err()
                .to_string()
                .starts_with("Invalid viz_node_limit in ")
        );
        fs::write(&rc, "no equals here\n").unwrap();
        assert!(
            load_rc_file(dir.path())
                .unwrap_err()
                .to_string()
                .ends_with(": 'no equals here' (expected key=value)")
        );
        fs::write(
            &rc,
            "# comment\nviz_node_limit=0\nfuture_option=whatever\nlanguage.tpl=.ts\n",
        )
        .unwrap();
        let config = load_rc_file(dir.path()).unwrap();
        assert_eq!(
            config.to_json(),
            json!({"viz_node_limit": 0, "languages": {".tpl": ".ts"}})
        );
        fs::write(&rc, "viz_node_limit=1_000\n").unwrap();
        assert_eq!(load_rc_file(dir.path()).unwrap().viz_node_limit, Some(1000));
    }

    #[test]
    fn effective_suffix_and_cache_salt_follow_the_declaration() {
        reset();
        assert_eq!(effective_suffix("x/a.inc"), ".inc");
        assert_eq!(effective_suffix("x/a.F90"), ".F90");
        assert_eq!(cache_salt("a.inc"), None);
        set_language_overrides(Some(&LanguageOverrides::from([(
            "inc".into(),
            "PHP".into(),
        )])));
        assert_eq!(effective_suffix("x/a.inc"), ".php");
        assert_eq!(effective_suffix("x/a.INC"), ".php");
        assert_eq!(effective_suffix("x/a.F90"), ".F90");
        assert_eq!(cache_salt("a.inc").as_deref(), Some("language=.php"));
        assert_eq!(cache_salt("b.php"), None);
        reset();
    }

    #[test]
    fn a_malformed_file_warns_once_and_activates_nothing() {
        reset();
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(RC_FILENAME),
            "language.tpl=.ts\nlanguage.inc=.phhp\n",
        )
        .unwrap();
        set_language_overrides(Some(&LanguageOverrides::from([(
            ".inc".into(),
            ".php".into(),
        )])));
        let (active, warnings) = capture_warnings(|| {
            let first = activate_language_overrides(dir.path());
            activate_language_overrides(dir.path());
            first
        });
        assert!(active.is_empty());
        assert_eq!(effective_suffix("a.inc"), ".inc");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].starts_with("[graphicle] warning: ignoring .graphiclerc: Invalid"));
        assert!(warnings[0].contains("unknown extension '.phhp'"));
        let empty = tempfile::tempdir().unwrap();
        set_language_overrides(Some(&LanguageOverrides::from([(
            ".inc".into(),
            ".php".into(),
        )])));
        assert!(activate_language_overrides(empty.path()).is_empty());
        assert_eq!(effective_suffix("a.inc"), ".inc");
        reset();
    }

    #[test]
    fn overrides_stay_with_their_thread_and_reach_workers() {
        let barrier = std::sync::Barrier::new(2);
        let seen = std::thread::scope(|scope| {
            let a = scope.spawn(|| {
                set_language_overrides(Some(&LanguageOverrides::from([(
                    ".inc".into(),
                    ".php".into(),
                )])));
                barrier.wait();
                effective_suffix("x.inc")
            });
            let b = scope.spawn(|| {
                set_language_overrides(Some(&LanguageOverrides::from([(
                    ".inc".into(),
                    ".pas".into(),
                )])));
                barrier.wait();
                effective_suffix("x.inc")
            });
            (a.join().unwrap(), b.join().unwrap())
        });
        assert_eq!(seen, (".php".to_owned(), ".pas".to_owned()));
        // A worker handed the caller's snapshot sees the caller's overrides,
        // whatever another thread set last.
        set_language_overrides(Some(&LanguageOverrides::from([(
            ".inc".into(),
            ".sql".into(),
        )])));
        let items = (0..64).collect::<Vec<_>>();
        let suffixes = crate::par::map(&items, 4, |_| effective_suffix("y.inc"));
        assert!(suffixes.iter().all(|s| s == ".sql"));
        reset();
        assert_eq!(effective_suffix("y.inc"), ".inc");
    }
}
