//! Which agent clients are installed, and whether Graphify is set up
//! (`graphicle setup`, `graphicle connect` without a client). Everything is
//! found from the clients' configuration folders and the programs on
//! `PATH`; no program is started and nothing is written.
use serde::Serialize;
use serde_json::Value;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The environment detection reads, so that tests can give their own.
#[derive(Clone, Debug, Default)]
pub struct Environment {
    pub home: Option<PathBuf>,
    pub path: Option<OsString>,
    /// `CLAUDE_CONFIG_DIR`: Claude Code's folder instead of `~/.claude`.
    pub claude_config_dir: Option<PathBuf>,
    /// `CODEX_HOME`: Codex's folder instead of `~/.codex`.
    pub codex_home: Option<PathBuf>,
    pub xdg_config_home: Option<PathBuf>,
    /// `%APPDATA%` (Windows; else `AppData\Roaming` under `home`).
    #[cfg(windows)]
    pub app_data: Option<PathBuf>,
    /// `%LOCALAPPDATA%` (Windows; else `AppData\Local` under `home`).
    #[cfg(windows)]
    pub local_app_data: Option<PathBuf>,
}

impl Environment {
    /// This process's environment.
    pub fn current() -> Self {
        let var = |name: &str| {
            std::env::var_os(name)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        };
        Self {
            home: crate::platform::home_dir().filter(|home| !home.as_os_str().is_empty()),
            path: std::env::var_os("PATH"),
            claude_config_dir: var("CLAUDE_CONFIG_DIR"),
            codex_home: var("CODEX_HOME"),
            xdg_config_home: var("XDG_CONFIG_HOME"),
            #[cfg(windows)]
            app_data: crate::platform::roaming_app_data(),
            #[cfg(windows)]
            local_app_data: crate::platform::local_app_data(),
        }
    }

    /// The first executable file named `name` in `PATH`'s absolute folders
    /// (on Windows, with each `PATHEXT` extension: npm installs `claude.cmd`).
    pub fn program(&self, name: &str) -> Option<PathBuf> {
        let names = crate::platform::program_names(name);
        std::env::split_paths(self.path.as_ref()?)
            .filter(|folder| folder.is_absolute())
            .flat_map(|folder| names.iter().map(move |name| folder.join(name)))
            .find(|candidate| executable(candidate))
    }

    /// `%APPDATA%` and `%LOCALAPPDATA%`, else their folders under `home`.
    #[cfg(windows)]
    fn app_data_dirs(&self) -> (Option<PathBuf>, Option<PathBuf>) {
        (
            self.app_data
                .clone()
                .or_else(|| self.at_home(r"AppData\Roaming")),
            self.local_app_data
                .clone()
                .or_else(|| self.at_home(r"AppData\Local")),
        )
    }

    /// `~/<relative>`.
    fn at_home(&self, relative: &str) -> Option<PathBuf> {
        self.home.as_ref().map(|home| home.join(relative))
    }

    /// Claude Code's own folder: `CLAUDE_CONFIG_DIR`, else `~/.claude`.
    pub fn claude_dir(&self) -> Option<PathBuf> {
        self.claude_config_dir
            .clone()
            .or_else(|| self.at_home(".claude"))
    }

    /// Codex's folder: `CODEX_HOME`, else `~/.codex`.
    pub fn codex_dir(&self) -> Option<PathBuf> {
        self.codex_home.clone().or_else(|| self.at_home(".codex"))
    }

    /// Codex's `config.toml`.
    pub fn codex_config(&self) -> Option<PathBuf> {
        crate::codex_config::config_path(self.codex_home.as_deref(), self.home.as_deref())
    }

    /// Cursor's user MCP file.
    pub fn cursor_config(&self) -> Option<PathBuf> {
        self.at_home(".cursor/mcp.json")
    }

    /// Muse Code's user settings file, which holds its MCP servers
    /// (`mcp_servers`).
    pub fn muse_config(&self) -> Option<PathBuf> {
        self.at_home(".config/muse/settings.json")
    }

    /// Claude Desktop's folder (where `connect claude` writes).
    pub fn claude_desktop_dir(&self) -> Option<PathBuf> {
        #[cfg(target_os = "macos")]
        {
            self.at_home("Library/Application Support/Claude")
        }
        #[cfg(windows)]
        {
            let (roaming, local) = self.app_data_dirs();
            crate::platform::claude_desktop_dir(roaming.as_deref(), local.as_deref())
        }
        #[cfg(all(not(target_os = "macos"), not(windows)))]
        {
            self.at_home(".config/Claude")
        }
    }

    /// Claude Desktop's configuration file.
    pub fn claude_desktop_config(&self) -> Option<PathBuf> {
        self.claude_desktop_dir()
            .map(|folder| folder.join("claude_desktop_config.json"))
    }

    /// VS Code's user settings folder.
    fn vscode_dir(&self) -> Option<PathBuf> {
        #[cfg(target_os = "macos")]
        {
            self.at_home("Library/Application Support/Code")
        }
        // Windows: `%APPDATA%\Code`, else the Insiders build's folder.
        #[cfg(windows)]
        {
            let roaming = self.app_data_dirs().0?;
            let stable = roaming.join("Code");
            let insiders = roaming.join("Code - Insiders");
            Some(if !stable.is_dir() && insiders.is_dir() {
                insiders
            } else {
                stable
            })
        }
        #[cfg(all(not(target_os = "macos"), not(windows)))]
        {
            self.xdg_config_home
                .as_ref()
                .map(|folder| folder.join("Code"))
                .or_else(|| self.at_home(".config/Code"))
        }
    }
}

#[cfg(unix)]
fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}
#[cfg(not(unix))]
fn executable(path: &Path) -> bool {
    path.is_file()
}

/// The clients `graphicle connect` supports, in the order setup lists them.
pub const CLIENTS: [(&str, &str); 6] = [
    ("claude-code", "Claude Code"),
    ("codex", "Codex"),
    ("cursor", "Cursor"),
    ("vscode", "VS Code"),
    ("claude", "Claude Desktop"),
    ("muse", "Muse Code"),
];

/// The display name of a client id.
pub fn name(client: &str) -> &'static str {
    CLIENTS
        .iter()
        .find(|(id, _)| *id == client)
        .map_or("Unknown client", |(_, name)| name)
}

/// The client a word names: its id, its name, or a common alias.
pub fn parse(word: &str) -> Option<&'static str> {
    let word: String = word
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    let id = match word.as_str() {
        "claudecode" | "cc" => "claude-code",
        "codex" | "codexcli" => "codex",
        "cursor" => "cursor",
        "vscode" | "code" | "visualstudiocode" | "copilot" => "vscode",
        "claude" | "claudedesktop" | "desktop" => "claude",
        "muse" | "musecode" => "muse",
        _ => return None,
    };
    Some(id)
}

/// An installed client, and what showed it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Found {
    pub client: &'static str,
    pub name: &'static str,
    pub evidence: PathBuf,
}

/// The installed clients `connect` supports, found by their configuration
/// folders, else their programs on `PATH`.
pub fn detect(env: &Environment) -> Vec<Found> {
    let folder = |path: Option<PathBuf>| path.filter(|path| path.is_dir());
    let mut found = Vec::new();
    for (client, label) in CLIENTS {
        let evidence = match client {
            "claude-code" => folder(env.claude_dir())
                .or_else(|| env.at_home(".claude.json").filter(|path| path.is_file()))
                .or_else(|| env.program("claude")),
            "codex" => folder(env.codex_dir()).or_else(|| env.program("codex")),
            "cursor" => folder(env.at_home(".cursor")).or_else(|| env.program("cursor")),
            "vscode" => folder(env.vscode_dir())
                .or_else(|| folder(env.at_home(".vscode")))
                .or_else(|| env.program("code")),
            "muse" => folder(env.at_home(".config/muse")).or_else(|| env.program("muse")),
            _ => folder(env.claude_desktop_dir()),
        };
        if let Some(evidence) = evidence {
            found.push(Found {
                client,
                name: label,
                evidence,
            });
        }
    }
    found
}

/// Whether an MCP entry name is Graphify's: `graphify`, or `graphify-…`.
pub fn graphify_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name == "graphify" || name.starts_with("graphify-") || name.starts_with("graphify_")
}

/// The lines `start..end` of the `## graphify` section of a CLAUDE.md or
/// AGENTS.md text (Graphify's own marker): its heading to the line before
/// the next heading of level 1 or 2 (`# `, `## `) or HTML comment line
/// (another tool's marker), outside fenced code blocks, without the blank
/// lines that end it. Whatever follows the section is never part of it.
pub fn graphify_section(text: &str) -> Option<(usize, usize)> {
    let lines: Vec<&str> = text.split('\n').collect();
    let start = lines
        .iter()
        .position(|line| line.trim().eq_ignore_ascii_case("## graphify"))?;
    let mut fence: Option<&str> = None;
    let mut end = lines.len();
    for (i, line) in lines.iter().enumerate().skip(start + 1) {
        let trimmed = line.trim_start();
        let marker = ["```", "~~~"]
            .into_iter()
            .find(|marker| trimmed.starts_with(marker));
        match (fence, marker) {
            (Some(open), Some(marker)) if marker == open => {
                fence = None;
                continue;
            }
            (Some(_), _) => continue,
            (None, Some(marker)) => {
                fence = Some(marker);
                continue;
            }
            (None, None) => {}
        }
        if line.starts_with("# ") || line.starts_with("## ") || trimmed.starts_with("<!--") {
            end = i;
            break;
        }
    }
    while end > start + 1 && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    Some((start, end))
}

/// A Graphify MCP entry in a client's configuration.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GraphifyEntry {
    pub client: &'static str,
    pub file: PathBuf,
    pub name: String,
    /// The JSON object holding it; `None` for Codex's TOML table.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// How to remove it by hand, when setup does not edit that file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by_hand: Option<String>,
}

/// What of Graphify is set up for a folder.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Graphify {
    /// `graphify-out/` in the folder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<PathBuf>,
    pub entries: Vec<GraphifyEntry>,
    /// CLAUDE.md or AGENTS.md files of the folder with a `## graphify`
    /// section.
    pub sections: Vec<PathBuf>,
    /// `~/.claude/skills/graphify`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill: Option<PathBuf>,
    /// A Claude Code settings file of the folder whose hooks run Graphify
    /// (`graphify claude install` adds a PreToolUse hook that points Claude
    /// at `graphify-out/` before every search). Found, never edited.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hook: Option<PathBuf>,
}

impl Graphify {
    pub fn found(&self) -> bool {
        self.output.is_some()
            || !self.entries.is_empty()
            || !self.sections.is_empty()
            || self.skill.is_some()
            || self.hook.is_some()
    }
    /// Whether switching changes anything (an entry or a section setup
    /// edits).
    pub fn switchable(&self) -> bool {
        !self.sections.is_empty() || self.entries.iter().any(|entry| entry.by_hand.is_none())
    }
}

/// A JSON object from a file (comments and trailing commas allowed, as in
/// VS Code's `mcp.json`), `None` when it is missing, too large or not a
/// JSON object.
pub fn json_file(path: &Path) -> Option<Value> {
    if path.is_symlink() {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() > 16 * 1024 * 1024 {
        return None;
    }
    crate::json_edit::parse(std::str::from_utf8(&bytes).ok()?).filter(Value::is_object)
}

/// Graphify's footprint for `folder`: its output folder, MCP entries in the
/// folder's and the user's client files, `## graphify` sections in the
/// folder's CLAUDE.md and AGENTS.md, and its Claude Code skill.
pub fn detect_graphify(env: &Environment, folder: &Path) -> Graphify {
    let mut found = Graphify::default();
    let output = folder.join("graphify-out");
    if output.is_dir() {
        found.output = Some(output);
    }
    let json_files: [(&'static str, Option<PathBuf>, &str); 4] = [
        ("claude-code", Some(folder.join(".mcp.json")), "mcpServers"),
        ("vscode", Some(folder.join(".vscode/mcp.json")), "servers"),
        ("cursor", env.cursor_config(), "mcpServers"),
        ("claude", env.claude_desktop_config(), "mcpServers"),
    ];
    for (client, file, group) in json_files {
        let Some(file) = file else { continue };
        let Some(document) = json_file(&file) else {
            continue;
        };
        for name in document[group]
            .as_object()
            .into_iter()
            .flat_map(|servers| servers.keys())
            .filter(|name| graphify_name(name))
        {
            found.entries.push(GraphifyEntry {
                client,
                file: file.clone(),
                name: name.clone(),
                group: Some(group.into()),
                by_hand: None,
            });
        }
    }
    if let Some(file) = env.codex_config()
        && let Ok(text) = std::fs::read_to_string(&file)
    {
        for table in crate::codex_config::tables(&text) {
            if graphify_name(&table.name) {
                found.entries.push(GraphifyEntry {
                    client: "codex",
                    file: file.clone(),
                    name: table.name,
                    group: None,
                    by_hand: None,
                });
            }
        }
    }
    // Claude Code's user and local servers live in ~/.claude.json, which
    // Claude Code rewrites while it runs: found, never edited.
    if let Some(file) = env.at_home(".claude.json")
        && let Some(document) = json_file(&file)
    {
        let user = document["mcpServers"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        let local = document["projects"][folder.to_string_lossy().as_ref()]["mcpServers"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        for (scope, servers) in [("user", user), ("local", local)] {
            for name in servers.keys().filter(|name| graphify_name(name)) {
                found.entries.push(GraphifyEntry {
                    client: "claude-code",
                    file: file.clone(),
                    name: name.clone(),
                    group: None,
                    by_hand: Some(format!("claude mcp remove {name} -s {scope}")),
                });
            }
        }
    }
    for name in ["AGENTS.md", "CLAUDE.md"] {
        let file = folder.join(name);
        if std::fs::read_to_string(&file).is_ok_and(|text| graphify_section(&text).is_some()) {
            found.sections.push(file);
        }
    }
    if let Some(skill) = env
        .claude_dir()
        .map(|folder| folder.join("skills/graphify"))
        && skill.is_dir()
    {
        found.skill = Some(skill);
    }
    for name in [".claude/settings.json", ".claude/settings.local.json"] {
        let file = folder.join(name);
        let runs_graphify = json_file(&file).is_some_and(|document| {
            document["hooks"]
                .as_object()
                .into_iter()
                .flat_map(|events| events.values())
                .filter_map(Value::as_array)
                .flatten()
                .filter_map(|group| group["hooks"].as_array())
                .flatten()
                .filter_map(|hook| hook["command"].as_str())
                .any(|command| command.to_ascii_lowercase().contains("graphify"))
        });
        if runs_graphify {
            found.hook = Some(file);
            break;
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn windows_clients_live_under_app_data() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("claude.cmd"), "@echo off\r\n").unwrap();
        let env = Environment {
            home: Some(dir.path().to_path_buf()),
            path: Some(bin.clone().into_os_string()),
            app_data: Some(dir.path().join("Roaming")),
            local_app_data: Some(dir.path().join("Local")),
            ..Environment::default()
        };
        assert_eq!(env.program("claude"), Some(bin.join("claude.cmd")));
        assert_eq!(
            env.claude_desktop_dir(),
            Some(dir.path().join(r"Roaming\Claude"))
        );
        std::fs::create_dir_all(dir.path().join(r"Roaming\Code - Insiders")).unwrap();
        assert_eq!(
            env.vscode_dir(),
            Some(dir.path().join(r"Roaming\Code - Insiders"))
        );
        std::fs::create_dir_all(dir.path().join(r"Roaming\Code")).unwrap();
        assert_eq!(env.vscode_dir(), Some(dir.path().join(r"Roaming\Code")));
    }

    fn home() -> (tempfile::TempDir, Environment) {
        let dir = tempfile::tempdir().unwrap();
        let env = Environment {
            home: Some(dir.path().to_path_buf()),
            path: Some(dir.path().join("bin").into_os_string()),
            ..Environment::default()
        };
        (dir, env)
    }

    #[test]
    fn clients_are_found_by_their_folders_or_programs() {
        let (dir, env) = home();
        assert!(detect(&env).is_empty());
        std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
        std::fs::create_dir_all(dir.path().join(".cursor")).unwrap();
        std::fs::create_dir_all(dir.path().join(".vscode")).unwrap();
        std::fs::create_dir_all(env.claude_desktop_dir().unwrap()).unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("codex"), "#!/bin/sh\n").unwrap();
        // A program that is not executable is not one.
        assert_eq!(
            detect(&env).iter().map(|f| f.client).collect::<Vec<_>>(),
            ["claude-code", "cursor", "vscode", "claude"]
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(bin.join("codex"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
            let found = detect(&env);
            assert_eq!(
                found.iter().map(|f| f.client).collect::<Vec<_>>(),
                ["claude-code", "codex", "cursor", "vscode", "claude"]
            );
            assert_eq!(found[1].evidence, bin.join("codex"));
        }
        assert_eq!(parse("Claude Desktop"), Some("claude"));
        assert_eq!(parse("claude-code"), Some("claude-code"));
        assert_eq!(parse("VS Code"), Some("vscode"));
        assert_eq!(parse("windsurf"), None);
    }

    #[test]
    fn graphify_is_found_in_entries_sections_output_and_skill() {
        let (dir, env) = home();
        let folder = dir.path().join("repo");
        std::fs::create_dir_all(folder.join("graphify-out")).unwrap();
        std::fs::write(
            folder.join(".mcp.json"),
            r#"{"mcpServers":{"graphify":{"command":"graphify","args":["--mcp"]},"other":{"command":"x"}}}"#,
        )
        .unwrap();
        std::fs::write(
            folder.join("CLAUDE.md"),
            "# Project\n\n## graphify\n\nUse graphify-out/.\n\n## Style\n\nTabs.\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join(".codex")).unwrap();
        std::fs::write(
            dir.path().join(".codex/config.toml"),
            "[mcp_servers.graphify]\ncommand = \"graphify\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join(".claude/skills/graphify")).unwrap();
        std::fs::write(
            dir.path().join(".claude.json"),
            r#"{"mcpServers":{"graphify":{"command":"graphify"}}}"#,
        )
        .unwrap();
        let found = detect_graphify(&env, &folder);
        assert!(found.found() && found.switchable());
        assert_eq!(found.output, Some(folder.join("graphify-out")));
        assert_eq!(found.sections, vec![folder.join("CLAUDE.md")]);
        assert_eq!(
            found
                .entries
                .iter()
                .map(|e| (e.client, e.by_hand.is_some()))
                .collect::<Vec<_>>(),
            [
                ("claude-code", false),
                ("codex", false),
                ("claude-code", true)
            ]
        );
        assert_eq!(
            found.entries[2].by_hand.as_deref(),
            Some("claude mcp remove graphify -s user")
        );
        assert!(found.skill.is_some());
        let text = "# Project\n\n## graphify\n\nUse graphify-out/.\n\n## Style\n";
        let (start, end) = graphify_section(text).unwrap();
        let lines: Vec<&str> = text.split('\n').collect();
        assert_eq!(lines[start..end], ["## graphify", "", "Use graphify-out/."]);
        // Graphify's PreToolUse hook in the project's Claude Code settings.
        std::fs::create_dir_all(folder.join(".claude")).unwrap();
        std::fs::write(
            folder.join(".claude/settings.json"),
            r#"{"hooks":{"PreToolUse":[{"matcher":"Glob|Grep","hooks":[{"type":"command","command":"[ -f graphify-out/graph.json ] && echo graphify || true"}]}]}}"#,
        )
        .unwrap();
        assert_eq!(
            detect_graphify(&env, &folder).hook,
            Some(folder.join(".claude/settings.json"))
        );
    }

    #[test]
    fn the_graphify_section_ends_at_the_next_heading_of_level_one_or_two() {
        let section = |text: &str| {
            let (start, end) = graphify_section(text).unwrap();
            text.split('\n').collect::<Vec<_>>()[start..end].join("\n")
        };
        // An H1 after the section is the team's, not Graphify's.
        assert_eq!(
            section("## graphify\n\nRead graphify-out/.\n\n# Team rules\n\nNever push to main.\n"),
            "## graphify\n\nRead graphify-out/."
        );
        // A comment line inside a code block does not end it.
        assert_eq!(
            section(
                "## graphify\n\n```sh\n# build the graph\ngraphify .\n```\n\n## Style\nTabs.\n"
            ),
            "## graphify\n\n```sh\n# build the graph\ngraphify .\n```"
        );
        // Another tool's marker ends it too.
        assert_eq!(
            section("## graphify\nUse it.\n<!-- other:start -->\nOther.\n"),
            "## graphify\nUse it."
        );
    }
}
