// Adapted from Graphify 0.9.77 (5c7b8479), Apache-2.0; see NOTICE.md.
//! Detection fixes ported from Graphify 0.9.76: the engine's own installed
//! skill folders and single-file installs are not indexed (c75bc16, 7e8ad53,
//! #4057), and neither are upstream's (`skills/graphify`, `graphify.*`).
use graphicle::detect::{DetectOptions, Detector, EncodingWarnings, IgnoreState};
use graphicle::install::Options;
use serde_json::json;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// `canonicalize` in the engine's plain spelling: on Windows without the
/// verbatim prefix (`\\?\C:\x` is `C:\x`); elsewhere `canonicalize` itself.
fn canonical(path: &Path) -> PathBuf {
    let path = path.canonicalize().unwrap();
    match path.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        Some(plain) if cfg!(windows) && plain.as_bytes().get(1) == Some(&b':') => plain.into(),
        _ => path,
    }
}

/// A non-hidden project root: the shape rule looks at the absolute path, and
/// tempfile names its directories `.tmpXXXX`.
struct Project {
    _dir: tempfile::TempDir,
    home: PathBuf,
    root: PathBuf,
}

impl Project {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = canonical(dir.path());
        let root = base.join("proj");
        let home = base.join("home");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        Self {
            _dir: dir,
            home,
            root,
        }
    }
    fn write(&self, rel: &str, text: &str) -> PathBuf {
        let path = self.root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    }
    fn relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }
    fn detected(&self) -> BTreeSet<String> {
        let out = Detector::default()
            .detect(&self.root, &DetectOptions::default())
            .unwrap();
        out["files"]
            .as_object()
            .unwrap()
            .values()
            .flat_map(|files| files.as_array().unwrap().iter())
            .map(|f| self.relative(Path::new(f.as_str().unwrap())))
            .collect()
    }
    fn ignored(&self) -> impl FnMut(&Path) -> bool {
        let mut warnings = EncodingWarnings::default();
        let mut state = IgnoreState::new(&self.root, &[], true, &mut warnings).unwrap();
        move |path| state.predicate(path, &mut warnings).unwrap()
    }
    fn collected(&self) -> BTreeSet<String> {
        graphicle::extract::collect_files(&self.root, false, None)
            .unwrap()
            .iter()
            .map(|p| self.relative(p))
            .collect()
    }
    fn options(&self) -> Options {
        Options {
            home: self.home.clone(),
            project_dir: self.root.clone(),
            project: true,
            windows: false,
            claude_config_dir: None,
            local_appdata: None,
            executable: Some("graphicle-graph".into()),
            strict: false,
        }
    }
}

fn noise(part: &str, parent: Option<&str>) -> bool {
    graphicle::detect::dispatch(
        &json!({"function": "_is_noise_dir", "part": part, "parent": parent}),
    )
    .unwrap()["value"]
        .as_bool()
        .unwrap()
}

#[test]
fn detect_skips_the_installed_skill_but_keeps_the_users_own() {
    let p = Project::new();
    p.write("auth.py", "def login():\n    return 1\n");
    let skill = graphicle::install::copy_skill("claude", &p.options()).unwrap();
    assert_eq!(skill, p.root.join(".claude/skills/graphicle/SKILL.md"));
    p.write(
        ".claude/skills/graphicle/references/x.md",
        "# Ref\n\nText.\n",
    );
    let mine = p.write(".claude/skills/deploy/SKILL.md", "# Deploy\n\nShip it.\n");

    let found = p.detected();
    assert!(found.contains("auth.py"), "{found:?}");
    assert!(
        found.contains(".claude/skills/deploy/SKILL.md"),
        "{found:?}"
    );
    assert!(
        !found.iter().any(|f| f.contains("/skills/graphicle/")),
        "{found:?}"
    );
    let mut ignored = p.ignored();
    assert!(ignored(&skill));
    assert!(!ignored(&mine));
    let collected = p.collected();
    assert!(collected.contains("auth.py"), "{collected:?}");
    assert!(
        !collected.iter().any(|f| f.contains("/skills/graphicle/")),
        "{collected:?}"
    );
}

#[test]
fn every_platform_skill_layout_is_pruned() {
    let p = Project::new();
    p.write("app.py", "def main():\n    return 1\n");
    let layouts = [
        ".claude/skills/graphicle",
        ".codex/skills/graphicle",
        ".agents/skills/graphicle",
        ".gemini/skills/graphicle",
        ".pi/agent/skills/graphicle",
        ".config/kilo/skills/graphicle",
        ".aider/graphicle",
        // Upstream's own skill folders stay pruned too.
        ".claude/skills/graphify",
        ".codex/skills/graphify",
        ".agents/skills/graphify",
        ".gemini/skills/graphify",
        ".pi/agent/skills/graphify",
        ".config/kilo/skills/graphify",
        ".aider/graphify",
    ];
    for layout in layouts {
        p.write(&format!("{layout}/SKILL.md"), "# graphicle\n\nSkill.\n");
        p.write(
            &format!("{layout}/references/query.md"),
            "# Query\n\nRef.\n",
        );
    }
    for name in graphicle::install::platforms()
        .into_iter()
        .filter(|n| !["cursor", "vscode"].contains(&n.as_str()))
    {
        let skill = graphicle::install::destination(&name, &p.options()).unwrap();
        if skill.starts_with(&p.root) {
            p.write(&p.relative(&skill), "# graphicle\n\nSkill.\n");
        }
    }
    assert_eq!(p.detected(), BTreeSet::from(["app.py".to_string()]));
    assert_eq!(p.collected(), BTreeSet::from(["app.py".to_string()]));
}

#[test]
fn source_trees_and_lookalike_folders_stay_detected() {
    let p = Project::new();
    let keep = [
        "graphicle/skill.md",
        "graphicle/skills/claude/references/query.md",
        "graphicle/skills/codex/references/update.md",
        "graphicle/detect.py",
        "skills/graphicle/SKILL.md",
        "src/skills/graphicle/loader.py",
        "docs/agent/skills/graphicle/notes.md",
        ".claude/skills/graphicle-extras/SKILL.md",
        ".claude/graphicle/notes.md",
        "graphify/skill.md",
        "graphify/detect.py",
        "skills/graphify/SKILL.md",
        "src/skills/graphify/loader.py",
        ".claude/skills/graphify-extras/SKILL.md",
        ".claude/graphify/notes.md",
    ];
    for rel in keep {
        let text = if rel.ends_with(".py") {
            "def f():\n    return 1\n"
        } else {
            "# Title\n\nText.\n"
        };
        p.write(rel, text);
    }
    let found = p.detected();
    let mut ignored = p.ignored();
    for rel in keep {
        assert!(found.contains(rel), "{rel} missing from {found:?}");
        assert!(!ignored(&p.root.join(rel)), "{rel}");
    }
}

#[test]
fn the_skill_folder_rule_needs_a_parent() {
    assert!(!noise("skills", None));
    for name in ["graphicle", "graphify"] {
        assert!(!noise(name, None));
        assert!(!noise(name, Some("../skills")));
        assert!(noise(name, Some("/repo/.claude/skills")));
        assert!(noise(name, Some("/repo/.aider")));
        assert!(noise(name, Some("/repo/.pi/agent/skills")));
        assert!(!noise(name, Some("/repo/src/skills")));
    }
}

/// Files an install edits rather than owns; they stay indexable.
const USER_FILES_INSTALL_EDITS: [&str; 12] = [
    "AGENTS.md",
    "CLAUDE.md",
    ".claude/CLAUDE.md",
    "GEMINI.md",
    "CODEBUDDY.md",
    ".github/copilot-instructions.md",
    ".claude/settings.json",
    ".codebuddy/settings.json",
    ".codex/hooks.json",
    ".gemini/settings.json",
    ".kilo/kilo.json",
    ".opencode/opencode.json",
];

const INSTALLED_FILES: [&str; 7] = [
    ".agents/rules/graphicle.md",
    ".agents/workflows/graphicle.md",
    ".cursor/rules/graphicle.mdc",
    ".kilo/plugins/graphicle.js",
    ".kiro/steering/graphicle.md",
    ".opencode/plugins/graphicle.js",
    ".windsurf/rules/graphicle.md",
];

/// The same files under upstream's names, still skipped when present.
const UPSTREAM_INSTALLED_FILES: [&str; 7] = [
    ".agents/rules/graphify.md",
    ".agents/workflows/graphify.md",
    ".cursor/rules/graphify.mdc",
    ".kilo/plugins/graphify.js",
    ".kiro/steering/graphify.md",
    ".opencode/plugins/graphify.js",
    ".windsurf/rules/graphify.md",
];

#[test]
fn detect_skips_every_file_an_install_writes() {
    let p = Project::new();
    p.write("app.py", "def main():\n    return 1\n");
    for rel in UPSTREAM_INSTALLED_FILES {
        p.write(rel, "# rule\n");
    }
    let options = p.options();
    for name in graphicle::install::platforms() {
        if name != "cursor" {
            graphicle::install::install(&name, &options).unwrap();
        }
        graphicle::install::configure(&name, &options, false).unwrap();
    }
    let edits = USER_FILES_INSTALL_EDITS
        .iter()
        .map(|s| s.to_string())
        .collect::<BTreeSet<_>>();

    let found = p.detected();
    assert!(found.contains("app.py"));
    let extra = found
        .iter()
        .filter(|f| *f != "app.py" && !edits.contains(*f))
        .collect::<Vec<_>>();
    assert!(extra.is_empty(), "{extra:?}");

    let mut ignored = p.ignored();
    for rel in INSTALLED_FILES {
        let written = p.root.join(rel);
        assert!(written.is_file(), "no installer writes {rel} any more");
        assert!(ignored(&written), "{rel}");
    }
    for rel in UPSTREAM_INSTALLED_FILES {
        assert!(ignored(&p.root.join(rel)), "{rel}");
    }
    let collected = p.collected();
    assert!(collected.contains("app.py"));
    let extra = collected
        .iter()
        .filter(|f| *f != "app.py" && !edits.contains(*f))
        .collect::<Vec<_>>();
    assert!(extra.is_empty(), "{extra:?}");
}

#[test]
fn detect_keeps_files_named_like_installed_files() {
    let p = Project::new();
    let keep = [
        "docs/graphicle.md",
        "rules/graphicle.md",
        "steering/graphicle.md",
        "src/plugins/graphicle.js",
        "plugins/graphicle.js",
        ".github/rules/graphicle.md",
        ".opencode/plugins/team.js",
        ".kiro/steering/product.md",
        ".agents/rules/style.md",
        ".windsurf/rules/graphicle-notes.md",
        "docs/graphify.md",
        "src/plugins/graphify.js",
        ".windsurf/rules/graphify-notes.md",
    ];
    for rel in keep {
        let text = if rel.ends_with(".js") {
            "export function f() { return 1 }\n"
        } else {
            "# Title\n\nText.\n"
        };
        p.write(rel, text);
    }
    let found = p.detected();
    let mut ignored = p.ignored();
    for rel in keep {
        assert!(found.contains(rel), "{rel} missing from {found:?}");
        assert!(!ignored(&p.root.join(rel)), "{rel}");
    }
    let collected = p.collected();
    for rel in [
        "src/plugins/graphicle.js",
        "plugins/graphicle.js",
        ".opencode/plugins/team.js",
        "src/plugins/graphify.js",
    ] {
        assert!(collected.contains(rel), "{rel} missing from {collected:?}");
    }
}
