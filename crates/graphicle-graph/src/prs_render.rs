// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/prs.py
use crate::{
    prs::{PRInfo, ordered},
    python_compat,
};
use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use std::{collections::BTreeMap, io::IsTerminal, sync::LazyLock};

#[derive(Clone, Debug)]
pub struct RenderContext {
    pub now_micros: i64,
    pub no_color: bool,
}
impl Default for RenderContext {
    fn default() -> Self {
        Self {
            now_micros: chrono::Utc::now().timestamp_micros(),
            no_color: !std::io::stdout().is_terminal()
                || std::env::var("NO_COLOR").is_ok_and(|s| !s.is_empty())
                || !virtual_terminal(),
        }
    }
}
/// Windows: colour only where the console accepts ANSI escapes; elsewhere
/// always true.
fn virtual_terminal() -> bool {
    #[cfg(windows)]
    {
        crate::win::enable_virtual_terminal()
    }
    #[cfg(not(windows))]
    {
        true
    }
}
impl RenderContext {
    pub fn color(&self, code: &str, text: &str) -> String {
        if self.no_color {
            text.into()
        } else {
            format!("\x1b[{code}m{text}\x1b[0m")
        }
    }
    pub fn status(&self, status: &str) -> String {
        match status {
            "READY" => self.color("32", status),
            "APPROVED" => self.color("1", &self.color("32", status)),
            "CI-FAIL" | "CHANGES-REQ" => self.color("31", status),
            "WRONG-BASE" | "STALE" => self.color("2", status),
            "DRAFT" | "PENDING" => self.color("33", status),
            _ => status.into(),
        }
    }
    pub fn ci(&self, status: &str) -> String {
        match status {
            "SUCCESS" => self.color("32", "✓"),
            "FAILURE" => self.color("31", "✗"),
            "PENDING" => self.color("33", "…"),
            "NONE" => self.color("2", "–"),
            _ => "?".into(),
        }
    }
}
#[derive(Default, Debug, Serialize)]
pub struct Output {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}
pub fn pad(s: &str, width: i64) -> String {
    static ANSI: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[[0-9;]*m").unwrap());
    let len = ANSI.replace_all(s, "").chars().count() as i64;
    format!("{s}{}", " ".repeat((width - len).max(0) as usize))
}
pub fn truncate(s: &str, n: i64) -> String {
    let len = s.chars().count() as i64;
    if len <= n {
        s.into()
    } else {
        let end = if n - 1 < 0 {
            (len + n - 1).max(0)
        } else {
            n - 1
        };
        format!("{}…", s.chars().take(end as usize).collect::<String>())
    }
}
pub fn render_dashboard(prs: &[PRInfo], base: &str, show_wrong: bool, c: &RenderContext) -> String {
    let prs_sorted = ordered(prs, base, c.now_micros);
    let mut wrong: Vec<_> = prs.iter().filter(|p| p.base_branch != base).collect();
    let mut out = format!(
        "\n{}\n\n",
        c.color(
            "1",
            &format!(
                "  {} prs  ·  base: {base}  ·  {} PRs",
                crate::cli::program(),
                prs_sorted.len()
            )
        )
    );
    if prs_sorted.is_empty() {
        out.push_str(&format!(
            "{}\n",
            c.color("2", "  No open PRs targeting this base branch.")
        ));
    } else {
        out.push_str(&format!(
            "  {:>4}  {:2}  {:13}  {:8}  {:22}  TITLE\n",
            "#", "CI", "STATUS", "UPDATED", "IMPACT"
        ));
        out.push_str(&format!(
            "  {}  {}  {}  {}  {}  {}\n",
            "─".repeat(4),
            "─".repeat(2),
            "─".repeat(13),
            "─".repeat(8),
            "─".repeat(8 + 14),
            "─".repeat(40)
        ));
        for pr in &prs_sorted {
            let status = pad(&c.status(pr.status(c.now_micros)), 13);
            let days = pr.days_old(c.now_micros);
            let age = if days > 0 {
                format!("{days}d")
            } else {
                "today".into()
            };
            let radius = pr.blast_radius();
            let impact = pad(
                &c.color(
                    "2",
                    &if radius.is_empty() {
                        "–".into()
                    } else {
                        truncate(&radius, 22)
                    },
                ),
                22,
            );
            let wt = if pr.worktree_path.as_ref().is_some_and(|s| !s.is_empty()) {
                format!(" {}", c.color("36", "⬡"))
            } else {
                "  ".into()
            };
            let draft = if pr.is_draft {
                c.color("2", " [draft]")
            } else {
                String::new()
            };
            let num = pad(&c.color("1", &format!("#{}", pr.number)), 6);
            out.push_str(&format!(
                "  {num}{wt}  {}  {status}  {age:>6}   {impact}  {}{draft}\n",
                c.ci(&pr.ci_status),
                truncate(&pr.title, 52)
            ));
        }
    }
    let mut counts = BTreeMap::new();
    for p in prs_sorted {
        *counts.entry(p.status(c.now_micros)).or_insert(0) += 1;
    }
    let mut parts = Vec::new();
    for (status, word, code) in [
        ("READY", "ready", "32"),
        ("APPROVED", "approved", "32"),
        ("PENDING", "pending CI", "33"),
        ("CI-FAIL", "CI failing", "31"),
        ("CHANGES-REQ", "changes requested", "31"),
        ("DRAFT", "draft", "33"),
        ("STALE", "stale", "2"),
    ] {
        if let Some(n) = counts.get(status) {
            let part = c.color(code, &format!("{n} {word}"));
            parts.push(if status == "APPROVED" {
                c.color("1", &part)
            } else {
                part
            });
        }
    }
    if !wrong.is_empty() {
        parts.push(c.color("2", &format!("{} wrong base", wrong.len())));
    }
    out.push_str(&format!("\n  {}\n\n", parts.join(" · ")));
    if !wrong.is_empty() && show_wrong {
        out.push_str(&format!(
            "{}\n",
            c.color(
                "2",
                &format!("  ── {} PRs targeting wrong base ──", wrong.len())
            )
        ));
        wrong.sort_by_key(|p| std::cmp::Reverse(p.number));
        for p in wrong {
            out.push_str(&format!(
                "{}\n",
                c.color(
                    "2",
                    &format!(
                        "  #{:4}  base={:12}  {}",
                        p.number,
                        p.base_branch,
                        truncate(&p.title, 60)
                    )
                )
            ));
        }
        out.push('\n');
    }
    out
}
pub fn render_worktrees(
    prs: &[PRInfo],
    worktrees: &BTreeMap<String, String>,
    c: &RenderContext,
) -> String {
    let mut out = format!("\n{}\n\n", c.color("1", "  Worktrees"));
    if worktrees.is_empty() {
        out.push_str(&format!(
            "{}\n\n",
            c.color("2", "  No active worktrees found.")
        ));
        return out;
    }
    for (branch, path) in worktrees {
        out.push_str(&format!("  {}\n", c.color("36", path)));
        if let Some(pr) = prs.iter().rev().find(|p| &p.branch == branch) {
            out.push_str(&format!(
                "    {} {branch}  ->  PR {}  [{}]  {}\n",
                c.color("2", "branch:"),
                c.color("1", &format!("#{}", pr.number)),
                c.status(pr.status(c.now_micros)),
                truncate(&pr.title, 50)
            ));
        } else {
            out.push_str(&format!(
                "    {} {branch}  {}\n",
                c.color("2", "branch:"),
                c.color("2", "(no open PR)")
            ));
        }
        out.push('\n');
    }
    out
}
pub fn render_conflicts(prs: &[PRInfo], base: &str, labels: &Value, c: &RenderContext) -> String {
    let mut communities: Vec<(i64, Vec<&PRInfo>)> = Vec::new();
    for p in prs
        .iter()
        .filter(|p| p.base_branch == base && !p.communities_touched.is_empty())
    {
        for id in &p.communities_touched {
            if let Some((_, ps)) = communities.iter_mut().find(|(n, _)| n == id) {
                ps.push(p);
            } else {
                communities.push((*id, vec![p]));
            }
        }
    }
    if communities.is_empty() {
        return format!(
            "{}\n",
            c.color(
                "2",
                "\n  No graph impact data - run with a valid graph.json to detect conflicts.\n"
            )
        );
    }
    communities.retain(|(_, ps)| ps.len() > 1);
    if communities.is_empty() {
        return format!(
            "{}\n",
            c.color(
                "32",
                "\n  No community overlap between open PRs - safe to merge in any order.\n"
            )
        );
    }
    let mut out = format!(
        "\n{}\n\n",
        c.color(
            "1",
            "  Community conflicts (PRs sharing the same graph community)"
        )
    );
    communities.sort_by_key(|(_, ps)| std::cmp::Reverse(ps.len()));
    for (id, prs) in communities {
        let names = labels[&id.to_string()]
            .as_array()
            .map(|a| a.iter().map(python_compat::string).collect::<Vec<_>>())
            .unwrap_or_default();
        let label = if names.is_empty() {
            String::new()
        } else {
            c.color("2", &format!("  — {}", names.join(", ")))
        };
        out.push_str(&format!(
            "  {}{label}  ({} PRs overlap)\n",
            c.color("33", &format!("Community {id}")),
            prs.len()
        ));
        for pr in prs {
            out.push_str(&format!(
                "    #{:4}  {}  {}\n",
                pr.number,
                pad(&c.status(pr.status(c.now_micros)), 13),
                truncate(&pr.title, 55)
            ));
        }
        out.push('\n');
    }
    out
}
pub fn render_pr_detail(pr: &PRInfo, c: &RenderContext) -> String {
    let mut out = format!(
        "\n{}\n  {}\n\n",
        c.color(
            "1",
            &format!(
                "  PR #{}  ·  {}",
                pr.number,
                c.status(pr.status(c.now_micros))
            )
        ),
        pr.title
    );
    out.push_str(&format!(
        "  {}  {}  ->  {}\n  {}  {}\n  {} {}d ago\n  {}      {} {}\n",
        c.color("2", "branch:"),
        pr.branch,
        pr.base_branch,
        c.color("2", "author:"),
        pr.author,
        c.color("2", "updated:"),
        pr.days_old(c.now_micros),
        c.color("2", "CI:"),
        c.ci(&pr.ci_status),
        pr.ci_status
    ));
    if !pr.review_decision.is_empty() {
        out.push_str(&format!(
            "  {} {}\n",
            c.color("2", "review:"),
            pr.review_decision
        ));
    }
    if let Some(path) = pr.worktree_path.as_ref().filter(|s| !s.is_empty()) {
        out.push_str(&format!(
            "  {} {}\n",
            c.color("2", "worktree:"),
            c.color("36", path)
        ));
    }
    let radius = pr.blast_radius();
    if !radius.is_empty() {
        out.push_str(&format!(
            "\n  {}  {radius}\n  {} {}\n",
            c.color("1", "Graph impact:"),
            c.color("2", "communities:"),
            python_compat::repr(&serde_json::json!(pr.communities_touched))
        ));
        if !pr.files_changed.is_empty() {
            out.push_str(&format!(
                "  {} {}\n",
                c.color("2", "files changed:"),
                pr.files_changed.len()
            ));
            for f in pr.files_changed.iter().take(10) {
                out.push_str(&format!("    {}\n", c.color("2", f)));
            }
            if pr.files_changed.len() > 10 {
                out.push_str(&format!(
                    "{}\n",
                    c.color(
                        "2",
                        &format!("    … and {} more", pr.files_changed.len() - 10)
                    )
                ));
            }
        }
    }
    out.push('\n');
    out
}
