//! The new-release notice. After a command people run at a terminal, at most
//! once a day, Graphicle asks GitHub for its latest release and, when that
//! release is newer than this program, prints on standard error:
//!
//! ```text
//! A new release of graphicle is available: 0.1.1 → 0.1.2
//! To upgrade, run: brew upgrade graphicle
//! https://github.com/graphicle-ai/graphicle/releases/tag/v0.1.2
//! ```
//!
//! - **What is sent.** One GET of [`LATEST`] (GitHub's releases API), with
//!   the User-Agent `graphicle` and nothing else: no code, project data,
//!   version or credentials. The answer's `tag_name` and `html_url` are kept
//!   in `update-check.json` in the cache folder ([`state_path`]).
//! - **When.** The check runs on its own thread while the command runs, with
//!   a 2-second timeout ([`TIMEOUT`]). After the command, the program waits
//!   for it at most [`WAIT`]; a check that has not finished by then is
//!   dropped. Each check is recorded before its request is sent, so a dropped
//!   check still counts as that day's check, though not as a failure. A check
//!   whose request fails, or that GitHub refuses or rate-limits, prints
//!   nothing and counts as a failure. The next check waits a day; after
//!   failures in a row it waits one, two, then at most four days
//!   ([`backoff`]). The notice itself is printed at most once a day, from the
//!   last answer.
//! - **Never** ([`allowed`]): when standard error is not a terminal, on CI
//!   ([`CI_ENV`]), with [`OPT_OUT_ENV`] set to any non-empty value, with
//!   `update_notifier: false` in the configuration ([`SETTING`]), in a
//!   development build, and after any command `crate::cli` does not call
//!   interactive: never after the servers (`mcp`, `serve`, `open`), hooks,
//!   session capture, workers, JSON output, `graphicle graph …` or `update`.
//!
//! Nothing is checked in a test's process (only [`crate::cli::main_with`]
//! calls [`allow`]), nor by an edition other than Core: GitHub's releases
//! are Core's.
use crate::{local, platform::CanonicalExt, terminal::Install};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    cmp::Ordering,
    ffi::OsString,
    io::{IsTerminal, Read as _, Write as _},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering as AtomicOrdering},
        mpsc,
    },
    time::Duration,
};

/// GitHub's latest-release endpoint for Graphicle (drafts and pre-releases
/// are never the latest release there).
pub const LATEST: &str = "https://api.github.com/repos/Graphicle-AI/graphicle/releases/latest";
/// Any non-empty value turns the check and the notice off.
pub const OPT_OUT_ENV: &str = "GRAPHICLE_NO_UPDATE_NOTIFIER";
/// The configuration key: `graphicle config set update_notifier false`.
pub const SETTING: &str = "update_notifier";
/// Environment variables that mark a CI run (any non-empty value).
pub const CI_ENV: &[&str] = &[
    "CI",
    "CONTINUOUS_INTEGRATION",
    "BUILD_NUMBER",
    "RUN_ID",
    "GITHUB_ACTIONS",
    "GITLAB_CI",
    "BUILDKITE",
    "CIRCLECI",
    "TRAVIS",
    "JENKINS_URL",
    "TEAMCITY_VERSION",
    "TF_BUILD",
    "CODEBUILD_BUILD_ID",
    "BITBUCKET_BUILD_NUMBER",
    "APPVEYOR",
    "DRONE",
];
/// The whole request's time limit.
pub const TIMEOUT: Duration = Duration::from_secs(2);
/// The most a command waits for an unfinished check after it is done.
pub const WAIT: Duration = Duration::from_millis(200);
/// How long a check, and a printed notice, last.
const DAY: i64 = 24 * 60 * 60;
/// The longest wait after failed checks ([`backoff`]).
const MAX_BACKOFF: i64 = 4 * DAY;
/// The state file's name in the cache folder.
const STATE_FILE: &str = "update-check.json";

static ALLOWED: AtomicBool = AtomicBool::new(false);
static PENDING: Mutex<Option<Pending>> = Mutex::new(None);

/// What [`start`] leaves for [`finish`].
struct Pending {
    path: PathBuf,
    /// The configuration file, read again after the command.
    config: PathBuf,
    /// The state before the command.
    state: State,
    /// The check's state, when [`start`] began one.
    check: Option<mpsc::Receiver<State>>,
}

/// A release: its version (without the tag's `v`) and its page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub url: String,
}

impl Release {
    /// The release in GitHub's answer: `tag_name` a version, `html_url` a page
    /// on github.com, both plain printable text; never a draft or a
    /// pre-release.
    pub fn from_api(value: &Value) -> Option<Self> {
        if value["draft"] == true || value["prerelease"] == true {
            return None;
        }
        let tag = value["tag_name"].as_str()?;
        let version = tag.strip_prefix('v').unwrap_or(tag);
        parse(version)?;
        let url = value["html_url"].as_str()?;
        if url.len() > 300
            || !url.starts_with("https://github.com/")
            || !url.bytes().all(|b| b.is_ascii_graphic())
        {
            return None;
        }
        Some(Self {
            version: version.to_string(),
            url: url.to_string(),
        })
    }
}

/// `update-check.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// When the latest release was last asked for (Unix seconds), answered
    /// or not.
    #[serde(default)]
    pub checked_at: i64,
    /// The checks in a row since the last answer whose request failed, was
    /// refused or was rate-limited (a dropped check does not count); 0 after
    /// an answer.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub failures: u32,
    /// The latest release's version then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest: Option<String>,
    /// Its page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// When the notice was last printed (Unix seconds).
    #[serde(default)]
    pub notified_at: i64,
}

fn is_zero(count: &u32) -> bool {
    *count == 0
}

// ------------------------------------------------------------- versions

/// `major.minor.patch` with its pre-release identifiers (build metadata
/// dropped).
#[derive(Debug, PartialEq, Eq)]
struct Version {
    core: (u64, u64, u64),
    pre: Vec<String>,
}

impl Ord for Version {
    /// Semantic Versioning's order: a pre-release comes before its release;
    /// numeric identifiers compare as numbers and before alphanumeric ones.
    fn cmp(&self, other: &Self) -> Ordering {
        self.core
            .cmp(&other.core)
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    for (a, b) in self.pre.iter().zip(&other.pre) {
                        let order = match (a.parse::<u64>(), b.parse::<u64>()) {
                            (Ok(a), Ok(b)) => a.cmp(&b),
                            (Ok(_), Err(_)) => Ordering::Less,
                            (Err(_), Ok(_)) => Ordering::Greater,
                            (Err(_), Err(_)) => a.cmp(b),
                        };
                        if order != Ordering::Equal {
                            return order;
                        }
                    }
                    self.pre.len().cmp(&other.pre.len())
                }
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// `major.minor.patch[-pre][+build]`, a leading `v` allowed.
fn parse(text: &str) -> Option<Version> {
    let text = text.strip_prefix('v').unwrap_or(text);
    if text.is_empty()
        || text.len() > 64
        || !text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-+".contains(&b))
    {
        return None;
    }
    let text = text.split_once('+').map_or(text, |(version, _)| version);
    let (core, pre) = match text.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (text, None),
    };
    let numbers: Vec<u64> = core
        .split('.')
        .map(|part| part.parse().ok())
        .collect::<Option<_>>()?;
    let [major, minor, patch] = numbers[..] else {
        return None;
    };
    let pre: Vec<String> = pre
        .map(|pre| pre.split('.').map(str::to_string).collect())
        .unwrap_or_default();
    if pre.iter().any(String::is_empty) {
        return None;
    }
    Some(Version {
        core: (major, minor, patch),
        pre,
    })
}

/// Whether `latest` is a release (no pre-release suffix) newer than
/// `current`. False when either does not parse.
pub fn newer(latest: &str, current: &str) -> bool {
    match (parse(latest), parse(current)) {
        (Some(latest), Some(current)) => latest.pre.is_empty() && latest > current,
        _ => false,
    }
}

/// A development build: a debug build, or a version that does not parse,
/// is 0.0.0 or names `dev`.
pub fn development(version: &str, debug_build: bool) -> bool {
    debug_build
        || parse(version).is_none_or(|v| v.core == (0, 0, 0))
        || version.to_ascii_lowercase().contains("dev")
}

// ---------------------------------------------------------------- gates

/// What decides whether a command may check ([`allowed`]).
pub struct Gates<'a> {
    /// The command prints its readable view for people (`crate::cli`
    /// decides; never a server, hook, capture, worker, JSON or `update`).
    pub interactive: bool,
    pub stderr_terminal: bool,
    /// The environment.
    pub env: &'a dyn Fn(&str) -> Option<OsString>,
    /// `update_notifier` in the configuration ([`setting`]), read only when
    /// the other gates pass.
    pub setting: &'a dyn Fn() -> Option<bool>,
    /// This program's version.
    pub version: &'a str,
    pub debug_build: bool,
}

/// Whether the check, and the notice, may run.
pub fn allowed(gates: &Gates) -> bool {
    let set = |name: &str| (gates.env)(name).is_some_and(|value| !value.is_empty());
    gates.interactive
        && gates.stderr_terminal
        && !set(OPT_OUT_ENV)
        && !CI_ENV.iter().any(|&name| set(name))
        && !development(gates.version, gates.debug_build)
        && (gates.setting)() != Some(false)
}

/// `update_notifier` in the configuration file at `path`: `None` when the
/// file or the key is missing; `Some(false)` when the value is not `true`
/// or the file cannot be read as JSON, so nothing is checked then.
pub fn setting(path: &Path) -> Option<bool> {
    match crate::terminal::read_config(path) {
        Ok(document) => document
            .get(SETTING)
            .map(|value| value.as_bool().unwrap_or(false)),
        Err(_) => Some(false),
    }
}

/// Whether the notice is on for the configuration file at `path` in this
/// environment (`graphicle config get update_notifier`).
pub fn on(path: &Path) -> bool {
    std::env::var_os(OPT_OUT_ENV).is_none_or(|value| value.is_empty())
        && setting(path) != Some(false)
}

// ---------------------------------------------------------------- state

/// `update-check.json` in Graphicle's cache folder: `$XDG_CACHE_HOME`
/// (an absolute path), else `.cache` in the home folder, then `graphicle`;
/// the graph engine keeps its caches there too.
pub fn state_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            crate::platform::home_dir()
                .filter(|home| home.is_absolute())
                .map(|home| home.join(".cache"))
        })?;
    Some(base.join("graphicle").join(STATE_FILE))
}

/// The state at `path`; empty when it is missing, larger than 4 KiB or not
/// valid.
pub fn read_state(path: &Path) -> State {
    let mut bytes = Vec::new();
    let read = std::fs::File::open(path).and_then(|file| file.take(4097).read_to_end(&mut bytes));
    if read.is_err() || bytes.len() > 4096 {
        return State::default();
    }
    serde_json::from_slice(&bytes).unwrap_or_default()
}

fn write_state(path: &Path, state: &State) {
    if let Ok(bytes) = serde_json::to_vec(state) {
        let _ = local::write_private(path, &bytes);
    }
}

/// How long the next check waits after the last one, given the failed
/// checks in a row since the last answer: a day while there are none or
/// after one, two days after two, then four days ([`MAX_BACKOFF`]).
pub fn backoff(failures: u32) -> i64 {
    let doublings = failures.saturating_sub(1).min(2);
    (DAY << doublings).min(MAX_BACKOFF)
}

/// Whether the state's last check is older than its [`backoff`], or later
/// than `now` (the clock moved back).
pub fn due(state: &State, now: i64) -> bool {
    state.checked_at <= 0
        || now - state.checked_at >= backoff(state.failures)
        || state.checked_at > now + 60
}

/// One check of the state at `path`: when it is [`due`], ask `fetch` for the
/// latest release and record the answer. The attempt is written first, with
/// the failures as they were, so that a check the program drops at exit still
/// counts as a check but not as a failure. A failed fetch then counts as a
/// failure and keeps the last known release; an answer clears the failures.
/// Returns the state after it.
pub fn check(path: &Path, now: i64, fetch: &dyn Fn() -> Option<Release>) -> State {
    let mut state = read_state(path);
    if !due(&state, now) {
        return state;
    }
    state.checked_at = now;
    write_state(path, &state);
    match fetch() {
        Some(release) => {
            state.latest = Some(release.version);
            state.url = Some(release.url);
            state.failures = 0;
        }
        None => state.failures = state.failures.saturating_add(1),
    }
    // A notice another command printed meanwhile.
    state.notified_at = state.notified_at.max(read_state(path).notified_at);
    write_state(path, &state);
    state
}

// --------------------------------------------------------------- notice

/// The command that upgrades this program, from `graphicle update`'s
/// install method: `brew upgrade graphicle`, the install script's line
/// (install.sh, or install.ps1 in PowerShell on Windows), else `graphicle
/// update`, which lists the steps (source, cargo and container installs).
pub fn upgrade_command(install: &Install) -> String {
    match (install.method, install.commands.as_slice()) {
        ("homebrew", _) => "brew upgrade graphicle".into(),
        ("script", [line]) => line.clone(),
        _ => "graphicle update".into(),
    }
}

/// The notice for `state`, when its latest release is newer than `current`
/// and no notice was printed in the last day.
pub fn notice(state: &State, current: &str, now: i64, upgrade: &str) -> Option<String> {
    let latest = state.latest.as_deref()?;
    if !newer(latest, current) {
        return None;
    }
    let printed = state.notified_at;
    if printed > 0 && printed <= now + 60 && now - printed < DAY {
        return None;
    }
    let mut text = format!(
        "\nA new release of graphicle is available: {current} → {latest}\nTo upgrade, run: {upgrade}\n"
    );
    if let Some(url) = &state.url {
        text.push_str(url);
        text.push('\n');
    }
    Some(text)
}

// ------------------------------------------------------------- the run

/// Lets this process check ([`crate::cli::main_with`] calls it): only the
/// program itself, never a test running the command line in-process.
pub fn allow() {
    ALLOWED.store(true, AtomicOrdering::Relaxed);
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

/// Before a command runs: when [`allowed`], start a check on its own thread
/// if one is [`due`]. `interactive`: the command prints its readable view
/// for people (`crate::cli` decides); `config`: the configuration file.
pub fn start(interactive: bool, config: &Path) {
    // GitHub's releases are Graphicle Core's; another edition updates its own way.
    if !ALLOWED.load(AtomicOrdering::Relaxed) || crate::core_capabilities::edition() != "core" {
        return;
    }
    let gates = Gates {
        interactive,
        stderr_terminal: std::io::stderr().is_terminal(),
        env: &|name| std::env::var_os(name),
        setting: &|| setting(config),
        version: env!("CARGO_PKG_VERSION"),
        debug_build: cfg!(debug_assertions),
    };
    if !allowed(&gates) {
        return;
    }
    let Some(path) = state_path() else {
        return;
    };
    let now = now();
    let state = read_state(&path);
    let check = if due(&state, now) {
        spawn_check(path.clone(), now)
    } else {
        None
    };
    if let Ok(mut pending) = PENDING.lock() {
        *pending = Some(Pending {
            path,
            config: config.to_path_buf(),
            state,
            check,
        });
    }
}

/// The check on its own thread with its own small runtime, so that the
/// program never waits for it at exit: a request (or a name lookup) still
/// running then ends with the process.
fn spawn_check(path: PathBuf, now: i64) -> Option<mpsc::Receiver<State>> {
    let (send, receive) = mpsc::channel();
    std::thread::Builder::new()
        .name("graphicle-update-check".into())
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            let state = check(&path, now, &|| runtime.block_on(latest_release()));
            let _ = send.send(state);
            runtime.shutdown_background();
        })
        .ok()?;
    Some(receive)
}

/// GitHub's latest release of Graphicle, or `None` on any error.
async fn latest_release() -> Option<Release> {
    let response = crate::model::client_with_timeout(TIMEOUT)
        .ok()?
        .get(LATEST)
        .header(reqwest::header::USER_AGENT, "graphicle")
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .ok()?;
    Release::from_api(&crate::model::bounded_json(response).await.ok()?)
}

/// After a command that succeeded: wait at most [`WAIT`] for the check
/// [`start`] began, then print the notice on standard error when one is due,
/// and record when. Silent on any error.
pub fn finish() {
    let Some(pending) = PENDING.lock().ok().and_then(|mut pending| pending.take()) else {
        return;
    };
    // The command may have turned the notice off (`config set update_notifier false`).
    if setting(&pending.config) == Some(false) {
        return;
    }
    let state = match pending.check {
        Some(check) => check.recv_timeout(WAIT).unwrap_or(pending.state),
        None => pending.state,
    };
    let now = now();
    let exe = std::env::current_exe()
        .ok()
        .map(|exe| exe.canonical().unwrap_or(exe))
        .unwrap_or_default();
    let upgrade = upgrade_command(&crate::terminal::installed(&exe));
    let Some(text) = notice(&state, env!("CARGO_PKG_VERSION"), now, &upgrade) else {
        return;
    };
    if std::io::stderr().lock().write_all(text.as_bytes()).is_err() {
        return;
    }
    let mut saved = read_state(&pending.path);
    if saved.checked_at < state.checked_at {
        saved = state;
    }
    saved.notified_at = now;
    write_state(&pending.path, &saved);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::Cell;

    const NOW: i64 = 1_792_000_000;

    fn release(version: &str) -> Release {
        Release {
            version: version.into(),
            url: format!("https://github.com/graphicle-ai/graphicle/releases/tag/v{version}"),
        }
    }

    #[test]
    fn versions_compare_as_semantic_versions_with_pre_releases() {
        assert!(newer("0.1.2", "0.1.1"));
        assert!(newer("v1.2.0", "1.1.9"));
        assert!(newer("0.1.10", "0.1.9"), "numbers, not text");
        assert!(newer("1.0.0", "0.99.99"));
        assert!(!newer("0.1.1", "0.1.1"));
        assert!(!newer("0.1.0", "0.1.1"));
        assert!(
            !newer("0.1.1+build.7", "0.1.1"),
            "build metadata is ignored"
        );
        // A release is newer than its own pre-release; a pre-release is never
        // offered.
        assert!(newer("0.1.2", "0.1.2-rc.1"));
        assert!(!newer("0.1.2-rc.1", "0.1.1"));
        assert!(!newer("0.1.3-rc.1", "0.1.2-rc.1"));
        // Semantic Versioning's own example order.
        let order = [
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0",
        ];
        for pair in order.windows(2) {
            assert!(parse(pair[0]) < parse(pair[1]), "{} < {}", pair[0], pair[1]);
        }
        for bad in [
            "",
            "1.2",
            "1.2.3.4",
            "1.2.x",
            "1.2.3-",
            "1.2.3-a..b",
            "1.2.3\x1b[0m",
        ] {
            assert!(parse(bad).is_none(), "{bad:?}");
            assert!(!newer(bad, "0.1.1") && !newer("9.9.9", bad), "{bad:?}");
        }
    }

    #[test]
    fn development_builds_are_recognised() {
        assert!(!development("0.1.1", false));
        assert!(!development("0.1.2-rc.1", false));
        assert!(development("0.1.1", true), "a debug build");
        assert!(development("0.1.2-dev", false));
        assert!(development("0.1.2-dev.3", false));
        assert!(development("0.1.2+dev", false));
        assert!(development("0.0.0", false));
        assert!(development("main", false));
    }

    #[test]
    fn each_gate_turns_the_check_off() {
        fn gates_with(
            env: &[(&str, &str)],
            change: impl FnOnce(&mut (bool, bool, Option<bool>, &'static str, bool)),
        ) -> bool {
            let mut values = (true, true, None, "0.1.1", false);
            change(&mut values);
            let (interactive, stderr_terminal, setting, version, debug_build) = values;
            let env: Vec<(String, OsString)> = env
                .iter()
                .map(|(name, value)| (name.to_string(), OsString::from(value)))
                .collect();
            allowed(&Gates {
                interactive,
                stderr_terminal,
                env: &|name| {
                    env.iter()
                        .find(|(set, _)| set == name)
                        .map(|(_, value)| value.clone())
                },
                setting: &|| setting,
                version,
                debug_build,
            })
        }
        assert!(gates_with(&[], |_| {}), "every gate open");
        assert!(gates_with(&[("HOME", "/home/me")], |_| {}));
        assert!(!gates_with(&[], |g| g.0 = false), "not interactive");
        assert!(!gates_with(&[], |g| g.1 = false), "stderr not a terminal");
        assert!(!gates_with(&[], |g| g.2 = Some(false)), "setting off");
        assert!(gates_with(&[], |g| g.2 = Some(true)));
        assert!(
            !gates_with(&[], |g| g.3 = "0.1.2-dev"),
            "development version"
        );
        assert!(!gates_with(&[], |g| g.4 = true), "debug build");
        for value in ["1", "0", "true", "no"] {
            assert!(!gates_with(&[(OPT_OUT_ENV, value)], |_| {}), "{value}");
        }
        assert!(gates_with(&[(OPT_OUT_ENV, "")], |_| {}), "empty is unset");
        for name in CI_ENV {
            assert!(!gates_with(&[(*name, "true")], |_| {}), "{name}");
            assert!(gates_with(&[(*name, "")], |_| {}), "{name} empty");
        }
        // The configuration is read only when every other gate passes.
        let read = Cell::new(false);
        let closed = allowed(&Gates {
            interactive: false,
            stderr_terminal: true,
            env: &|_| None,
            setting: &|| {
                read.set(true);
                None
            },
            version: "0.1.1",
            debug_build: false,
        });
        assert!(!closed && !read.get());
    }

    #[test]
    fn the_setting_is_read_from_the_configuration_file() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("models.json");
        assert_eq!(setting(&path), None, "no file");
        std::fs::write(&path, r#"{"freshness":"off"}"#).unwrap();
        assert_eq!(setting(&path), None, "no key");
        std::fs::write(&path, r#"{"update_notifier":false}"#).unwrap();
        assert_eq!(setting(&path), Some(false));
        std::fs::write(&path, r#"{"update_notifier":true}"#).unwrap();
        assert_eq!(setting(&path), Some(true));
        std::fs::write(&path, r#"{"update_notifier":"yes"}"#).unwrap();
        assert_eq!(setting(&path), Some(false), "not a boolean");
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(setting(&path), Some(false), "unreadable: no check");
        // `graphicle config set update_notifier false` writes a valid file.
        let config: crate::model::Config =
            serde_json::from_value(json!({"update_notifier": false})).unwrap();
        assert_eq!(config.update_notifier, Some(false));
        assert!(
            serde_json::from_value::<crate::model::Config>(json!({"update_notifier": "off"}))
                .is_err()
        );
        let plain = serde_json::to_value(crate::model::Config::default()).unwrap();
        assert!(plain.get(SETTING).is_none());
    }

    #[test]
    fn a_check_runs_at_most_once_a_day_and_failures_are_recorded() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("cache/graphicle").join(STATE_FILE);
        let calls = Cell::new(0);
        let answer = |release: Option<Release>| {
            let calls = &calls;
            move || {
                calls.set(calls.get() + 1);
                release.clone()
            }
        };
        // The first check asks and records the answer (creating the folder).
        let state = check(&path, NOW, &answer(Some(release("0.1.2"))));
        assert_eq!(calls.get(), 1);
        assert_eq!(state.latest.as_deref(), Some("0.1.2"));
        assert_eq!(state.checked_at, NOW);
        assert_eq!(read_state(&path), state);
        // Within a day: the cached answer, no request.
        let again = check(&path, NOW + DAY - 1, &answer(Some(release("0.1.3"))));
        assert_eq!(calls.get(), 1);
        assert_eq!(again.latest.as_deref(), Some("0.1.2"));
        // A day later: asked again; a failure counts as the check and keeps
        // the last answer.
        let failed = check(&path, NOW + DAY, &answer(None));
        assert_eq!(calls.get(), 2);
        assert_eq!(failed.latest.as_deref(), Some("0.1.2"));
        assert_eq!(failed.checked_at, NOW + DAY);
        check(&path, NOW + DAY + 10, &answer(Some(release("0.1.3"))));
        assert_eq!(calls.get(), 2, "the failure counted as the day's check");
        // A check recorded in the future (the clock moved back) is due.
        assert!(due(&read_state(&path), NOW));
        assert!(!due(&read_state(&path), NOW + DAY + 1));
        // A damaged or oversized file is a missing one.
        std::fs::write(&path, "{").unwrap();
        assert_eq!(read_state(&path), State::default());
        std::fs::write(&path, vec![b' '; 5000]).unwrap();
        assert_eq!(read_state(&path), State::default());
        let fresh = check(&path, NOW, &answer(Some(release("0.1.3"))));
        assert_eq!(calls.get(), 3);
        assert_eq!(fresh.latest.as_deref(), Some("0.1.3"));
    }

    #[test]
    fn failed_checks_back_off_from_one_day_to_four() {
        assert_eq!(backoff(0), DAY, "after an answer");
        assert_eq!(backoff(1), DAY);
        assert_eq!(backoff(2), 2 * DAY);
        assert_eq!(backoff(3), 4 * DAY);
        assert_eq!(backoff(4), MAX_BACKOFF);
        assert_eq!(backoff(u32::MAX), MAX_BACKOFF);
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join(STATE_FILE);
        let calls = Cell::new(0);
        let failing = || -> Option<Release> {
            calls.set(calls.get() + 1);
            None
        };
        // Each failure doubles the wait, up to four days; nothing is asked
        // before the wait is over.
        let mut at = NOW;
        for (failures, wait) in [
            (1, DAY),
            (2, 2 * DAY),
            (3, 4 * DAY),
            (4, 4 * DAY),
            (5, 4 * DAY),
        ] {
            let state = check(&path, at, &failing);
            assert_eq!((state.failures, state.checked_at), (failures, at));
            assert_eq!(calls.get(), failures);
            check(&path, at + wait - 1, &failing);
            assert_eq!(calls.get(), failures, "asked again before {wait} s");
            at += wait;
        }
        // An answer ends the back-off: the release is kept and the next
        // check is a day later.
        let answered = check(&path, at, &|| Some(release("0.1.3")));
        assert_eq!(answered.failures, 0);
        assert_eq!(answered.latest.as_deref(), Some("0.1.3"));
        assert!(!due(&answered, at + DAY - 1));
        assert!(due(&answered, at + DAY));
        // A failure after an answer keeps the release it knew.
        let failed = check(&path, at + DAY, &failing);
        assert_eq!(
            (failed.failures, failed.latest.as_deref()),
            (1, Some("0.1.3"))
        );
    }

    #[test]
    fn a_dropped_check_counts_as_a_check_but_not_as_a_failure() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join(STATE_FILE);
        check(&path, NOW, &|| Some(release("0.1.2")));
        // While the request runs, the file already holds the attempt, with
        // no failure counted: a program that exits now (the check dropped
        // after [`WAIT`]) asks again a day later, not after its next command.
        let seen = std::cell::RefCell::new(None);
        let failed = check(&path, NOW + DAY, &|| {
            *seen.borrow_mut() = Some(read_state(&path));
            None
        });
        let dropped = seen.into_inner().unwrap();
        assert_eq!((dropped.checked_at, dropped.failures), (NOW + DAY, 0));
        assert_eq!(dropped.latest.as_deref(), Some("0.1.2"));
        assert!(!due(&dropped, NOW + 2 * DAY - 1));
        assert!(due(&dropped, NOW + 2 * DAY));
        // The request did fail, so the finished check counts as a failure.
        assert_eq!(failed.failures, 1);
        assert_eq!(read_state(&path), failed);
        // A dropped check leaves earlier failures as they were: after two,
        // the next check still waits two days.
        let after_two = State {
            failures: 2,
            ..dropped
        };
        write_state(&path, &after_two);
        check(&path, NOW + 3 * DAY, &|| {
            let during = read_state(&path);
            assert_eq!((during.checked_at, during.failures), (NOW + 3 * DAY, 2));
            Some(release("0.1.3"))
        });
        assert_eq!(read_state(&path).failures, 0, "an answer clears them");
    }

    #[test]
    fn the_state_file_of_an_earlier_release_still_reads() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join(STATE_FILE);
        // 0.1.1 wrote no failure count.
        std::fs::write(
            &path,
            r#"{"checked_at":1792000000,"latest":"0.1.1","url":"https://github.com/graphicle-ai/graphicle/releases/tag/v0.1.1","notified_at":0}"#,
        )
        .unwrap();
        let state = read_state(&path);
        assert_eq!((state.checked_at, state.failures), (NOW, 0));
        assert_eq!(state.latest.as_deref(), Some("0.1.1"));
        // The count is written only while it is not zero.
        let failing = State {
            failures: 2,
            ..state.clone()
        };
        assert_eq!(serde_json::to_value(&failing).unwrap()["failures"], 2);
        assert!(
            serde_json::to_value(&state)
                .unwrap()
                .get("failures")
                .is_none()
        );
    }

    #[test]
    fn the_notice_names_both_versions_the_upgrade_command_and_the_page() {
        let state = State {
            checked_at: NOW,
            failures: 0,
            latest: Some("0.1.2".into()),
            url: Some("https://github.com/graphicle-ai/graphicle/releases/tag/v0.1.2".into()),
            notified_at: 0,
        };
        assert_eq!(
            notice(&state, "0.1.1", NOW, "brew upgrade graphicle").as_deref(),
            Some(
                "\nA new release of graphicle is available: 0.1.1 → 0.1.2\n\
                 To upgrade, run: brew upgrade graphicle\n\
                 https://github.com/graphicle-ai/graphicle/releases/tag/v0.1.2\n"
            )
        );
        // Not for the same or an older version, nor without an answer.
        assert_eq!(notice(&state, "0.1.2", NOW, "x"), None);
        assert_eq!(notice(&state, "0.1.3", NOW, "x"), None);
        assert_eq!(notice(&State::default(), "0.1.1", NOW, "x"), None);
        // At most once a day.
        let printed = |at| State {
            notified_at: at,
            ..state.clone()
        };
        assert_eq!(notice(&printed(NOW - 3600), "0.1.1", NOW, "x"), None);
        assert!(notice(&printed(NOW - DAY), "0.1.1", NOW, "x").is_some());
        assert!(
            notice(&printed(NOW + 3600), "0.1.1", NOW, "x").is_some(),
            "clock moved back"
        );
    }

    #[test]
    fn the_upgrade_command_follows_the_install_method() {
        let install = |exe: &str| {
            crate::terminal::install_method(
                Path::new(exe),
                Some(Path::new("/home/me")),
                Some(Path::new("/home/me/.cargo/bin")),
                false,
            )
        };
        assert_eq!(
            upgrade_command(&install(
                "/opt/homebrew/Cellar/graphicle/0.1.1/bin/graphicle"
            )),
            "brew upgrade graphicle"
        );
        assert_eq!(
            upgrade_command(&install("/home/linuxbrew/.linuxbrew/bin/graphicle")),
            "brew upgrade graphicle"
        );
        if cfg!(not(windows)) {
            assert_eq!(
                upgrade_command(&install("/home/me/.local/bin/graphicle")),
                "curl -fsSL https://github.com/graphicle-ai/graphicle/releases/latest/download/install.sh | sh"
            );
        }
        let powershell = Install {
            method: "script",
            how: String::new(),
            commands: vec![
                "irm https://github.com/graphicle-ai/graphicle/releases/latest/download/install.ps1 | iex"
                    .into(),
            ],
        };
        assert_eq!(upgrade_command(&powershell), powershell.commands[0]);
        assert_eq!(
            upgrade_command(&install("/home/me/.cargo/bin/graphicle")),
            "graphicle update"
        );
        let docker = crate::terminal::install_method(
            Path::new("/usr/local/bin/graphicle"),
            None,
            None,
            true,
        );
        assert_eq!(upgrade_command(&docker), "graphicle update");
    }

    #[test]
    fn only_a_github_release_with_a_version_and_a_page_is_taken() {
        let page = "https://github.com/graphicle-ai/graphicle/releases/tag/v0.1.2";
        assert_eq!(
            Release::from_api(
                &json!({"tag_name":"v0.1.2","html_url":page,"draft":false,"prerelease":false})
            ),
            Some(release("0.1.2"))
        );
        assert_eq!(
            Release::from_api(&json!({"tag_name":"0.1.2","html_url":page})).map(|r| r.version),
            Some("0.1.2".into())
        );
        for bad in [
            json!({"tag_name":"v0.1.2"}),
            json!({"html_url":page}),
            json!({"tag_name":"latest","html_url":page}),
            json!({"tag_name":"v0.1.2\u{1b}[31m","html_url":page}),
            json!({"tag_name":"v0.1.2","html_url":"http://github.com/x"}),
            json!({"tag_name":"v0.1.2","html_url":"https://example.com/graphicle"}),
            json!({"tag_name":"v0.1.2","html_url":"https://github.com/x\u{1b}]8;;"}),
            json!({"tag_name":"v0.1.2","html_url":"https://github.com/a b"}),
            json!({"tag_name":"v0.1.2","html_url":page,"draft":true}),
            json!({"tag_name":"v0.1.2","html_url":page,"prerelease":true}),
            // GitHub's answer when it rate-limits a client.
            json!({"message":"API rate limit exceeded for 192.0.2.1.","documentation_url":"https://docs.github.com/rest/overview/rate-limits-for-the-rest-api"}),
        ] {
            assert_eq!(Release::from_api(&bad), None, "{bad}");
        }
    }

    #[test]
    fn nothing_is_checked_or_printed_unless_the_program_allows_it() {
        // A test's process never called `allow`: start and finish do nothing.
        assert!(!ALLOWED.load(AtomicOrdering::Relaxed));
        start(true, Path::new("/nonexistent/models.json"));
        assert!(PENDING.lock().unwrap().is_none());
        finish();
    }
}
