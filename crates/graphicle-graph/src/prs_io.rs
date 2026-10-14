// Adapted from Graphify 0.9.88 (a9860f70), Apache-2.0; see NOTICE.md.
// Upstream: graphify/prs.py
use crate::{
    Result, fsutil, process,
    prs::{ImpactIndex, PRInfo, Timestamp, build_community_labels, parse_ci},
    security,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Completed {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}
pub trait Commands: Send + Sync {
    fn run(
        &self,
        program: &str,
        args: &[String],
        timeout: Duration,
        input: Option<&str>,
    ) -> Result<Completed>;
}
#[derive(Default)]
pub struct NativeCommands {
    pub cwd: Option<PathBuf>,
}
impl Commands for NativeCommands {
    fn run(
        &self,
        program: &str,
        args: &[String],
        timeout: Duration,
        input: Option<&str>,
    ) -> Result<Completed> {
        let mut command = Command::new(program);
        command.args(args);
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let out = process::capture_with_input(
            &mut command,
            timeout,
            input.map(|s| s.as_bytes().to_vec()),
        )
        .map_err(crate::Error::from)?;
        Ok(Completed {
            code: out.code,
            stdout: out.stdout,
            stderr: out.stderr,
        })
    }
}
pub fn gh(commands: &dyn Commands, args: &[String]) -> Option<Value> {
    let out = commands
        .run("gh", args, Duration::from_secs(30), None)
        .ok()?;
    if out.code != Some(0) {
        return None;
    }
    serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).ok()
}
pub fn detect_default_branch(commands: &dyn Commands, repo: Option<&str>) -> String {
    let mut args = vec!["repo".into(), "view".into()];
    if let Some(repo) = repo.filter(|s| !s.is_empty()) {
        args.push(repo.into());
    }
    args.extend(["--json".into(), "defaultBranchRef".into()]);
    if let Some(data) = gh(commands, &args)
        && let Some(branch) = data["defaultBranchRef"]["name"]
            .as_str()
            .filter(|s| !s.is_empty())
    {
        return branch.into();
    }
    if repo.is_none_or(|s| s.is_empty()) {
        let args = ["symbolic-ref".into(), "refs/remotes/origin/HEAD".into()];
        if let Ok(result) = commands.run("git", &args, Duration::from_secs(5), None)
            && result.code == Some(0)
        {
            let refname = String::from_utf8_lossy(&result.stdout);
            let refname = refname.trim_matches(crate::dedup_strings::whitespace);
            if !refname.is_empty() {
                return refname.rsplit('/').next().unwrap().into();
            }
        }
    }
    "main".into()
}
/// The fields every PR query asks `gh` for (Graphify 0.9.87).
pub const PR_JSON_FIELDS: &str = "number,title,state,headRefName,baseRefName,author,isDraft,reviewDecision,statusCheckRollup,updatedAt";
/// One PR record of `gh pr list` or `gh pr view` (Graphify 0.9.87,
/// `_parse_pr_dict`): missing optional fields take their defaults, and a
/// missing update time is now.
pub fn parse_pr_dict(item: &Value, expected_base: &str) -> Result<PRInfo> {
    let text = |k: &str| item[k].as_str().unwrap_or("").to_owned();
    let updated = item["updatedAt"].as_str().filter(|s| !s.is_empty());
    Ok(PRInfo {
        number: item["number"].as_i64().unwrap_or(0),
        title: text("title"),
        branch: text("headRefName"),
        base_branch: text("baseRefName"),
        author: if crate::build::truthy(&item["author"]) {
            item["author"]["login"].as_str().unwrap_or("?").into()
        } else {
            "?".into()
        },
        is_draft: item["isDraft"].as_bool().unwrap_or(false),
        review_decision: item["reviewDecision"].as_str().unwrap_or("").into(),
        ci_status: parse_ci(
            item["statusCheckRollup"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or(&[]),
        )
        .into(),
        updated_at: match updated {
            Some(raw) => Timestamp::parse(&raw.replace('Z', "+00:00"))?,
            None => Timestamp::now(),
        },
        expected_base: expected_base.into(),
        worktree_path: None,
        communities_touched: vec![],
        nodes_affected: 0,
        files_changed: vec![],
    })
}
pub fn fetch_prs(
    commands: &dyn Commands,
    repo: Option<&str>,
    base: Option<&str>,
    limit: i64,
) -> Result<Vec<PRInfo>> {
    let base = base
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| detect_default_branch(commands, repo));
    let mut args = vec![
        "pr".into(),
        "list".into(),
        "--state".into(),
        "open".into(),
        "--limit".into(),
        limit.to_string(),
        "--json".into(),
        PR_JSON_FIELDS.into(),
    ];
    if let Some(repo) = repo.filter(|s| !s.is_empty()) {
        args.extend(["--repo".into(), repo.into()]);
    }
    let data =
        gh(commands, &args).ok_or("gh CLI not found or not authenticated. Run: gh auth login")?;
    let raw = data.as_array().ok_or("GitHub PR list must be an array")?;
    raw.iter().map(|item| parse_pr_dict(item, &base)).collect()
}
/// One open PR by number, for `prs <N>` when it is not among the open PRs
/// listed (Graphify 0.9.87, #3829): `None` when `gh` has no such PR or it is
/// closed or merged.
pub fn fetch_pr(
    commands: &dyn Commands,
    number: i64,
    repo: Option<&str>,
    base: Option<&str>,
) -> Result<Option<PRInfo>> {
    let base = base
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| detect_default_branch(commands, repo));
    let mut args = vec![
        "pr".into(),
        "view".into(),
        number.to_string(),
        "--json".into(),
        PR_JSON_FIELDS.into(),
    ];
    if let Some(repo) = repo.filter(|s| !s.is_empty()) {
        args.extend(["--repo".into(), repo.into()]);
    }
    let Some(raw) = gh(commands, &args).filter(Value::is_object) else {
        return Ok(None);
    };
    if raw["state"] != "OPEN" {
        return Ok(None);
    }
    parse_pr_dict(&raw, &base).map(Some)
}
pub fn fetch_pr_files(commands: &dyn Commands, number: i64, repo: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "pr".into(),
        "diff".into(),
        number.to_string(),
        "--name-only".into(),
    ];
    if let Some(repo) = repo.filter(|s| !s.is_empty()) {
        args.extend(["--repo".into(), repo.into()]);
    }
    let Ok(out) = commands.run("gh", &args, Duration::from_secs(30), None) else {
        return vec![];
    };
    if out.code != Some(0) {
        return vec![];
    }
    fsutil::split_lines(&String::from_utf8_lossy(&out.stdout))
        .map(|s| s.trim_matches(crate::dedup_strings::whitespace))
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}
pub fn parse_worktrees(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut current = None;
    for line in fsutil::split_lines(text) {
        if line.is_empty() {
            current = None;
        } else if let Some(path) = line.strip_prefix("worktree ") {
            current = Some(path.to_owned());
        } else if let Some(branch) = line.strip_prefix("branch refs/heads/")
            && let Some(path) = current.as_ref().filter(|s| !s.is_empty())
        {
            out.insert(branch.into(), path.clone());
        }
    }
    out
}
pub fn fetch_worktrees(commands: &dyn Commands) -> BTreeMap<String, String> {
    match commands.run(
        "git",
        &["worktree".into(), "list".into(), "--porcelain".into()],
        Duration::from_secs(10),
        None,
    ) {
        Ok(out) if out.code == Some(0) => parse_worktrees(&String::from_utf8_lossy(&out.stdout)),
        _ => BTreeMap::new(),
    }
}
pub fn load_graph_json(path: &Path, cap: Option<u64>) -> Option<Value> {
    if !path.exists() {
        return None;
    }
    security::check_graph_file_size_cap_with(
        path,
        cap.unwrap_or_else(security::max_graph_file_bytes),
    )
    .ok()?;
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}
/// Graph impact of each PR's changed files, fetched concurrently; PRs with
/// the wrong base are skipped unless `include_wrong_base` (a `prs <N>` detail
/// view, Graphify 0.9.87).
pub fn attach_graph_impact(
    commands: &dyn Commands,
    prs: &mut [PRInfo],
    path: &Path,
    repo: Option<&str>,
    now: i64,
    cap: Option<u64>,
    include_wrong_base: bool,
) -> Result<Value> {
    let Some(data) = load_graph_json(path, cap).filter(crate::build::truthy) else {
        return Ok(serde_json::json!({}));
    };
    let index = ImpactIndex::from_nodes(data["nodes"].as_array().into_iter().flatten())?;
    let jobs: Vec<_> = prs
        .iter()
        .enumerate()
        .filter(|(_, p)| include_wrong_base || p.status(now) != "WRONG-BASE")
        .map(|(i, p)| (i, p.number))
        .collect();
    let next = AtomicUsize::new(0);
    let batches = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for _ in 0..jobs.len().min(8) {
            handles.push(scope.spawn(|| {
                let mut results = Vec::new();
                loop {
                    let j = next.fetch_add(1, Ordering::Relaxed);
                    let Some((i, number)) = jobs.get(j) else {
                        break;
                    };
                    results.push((*i, fetch_pr_files(commands, *number, repo)));
                }
                results
            }));
        }
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_default())
            .collect::<Vec<_>>()
    });
    for batch in batches {
        for (i, files) in batch {
            let (communities, nodes) = index.impact(&files);
            prs[i].files_changed = files;
            prs[i].communities_touched = communities;
            prs[i].nodes_affected = nodes;
        }
    }
    build_community_labels(&data, 4)
}
