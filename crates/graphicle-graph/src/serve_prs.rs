// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/__main__.py, graphify/serve.py
//! Read-only pull-request tools for the native MCP service.
use crate::{Result, graph::Graph, prs, serve_search::text};
use serde_json::Value;
pub fn call(g: &Graph, name: &str, args: &Value, commands: &dyn prs::Commands) -> Result<String> {
    let repo = args["repo"].as_str().filter(|s| !s.is_empty());
    let now = chrono::Utc::now().timestamp_micros();
    if name == "get_pr_impact" {
        let number = args["pr_number"].as_i64().ok_or("pr_number is required")?;
        let mut view=vec!["pr".into(),"view".into(),number.to_string(),"--json".into(),"title,headRefName,baseRefName,author,isDraft,reviewDecision,statusCheckRollup,updatedAt".into()];
        if let Some(repo) = repo {
            view.extend(["--repo".into(), repo.into()]);
        }
        let pr = prs::gh(commands, &view)
            .ok_or_else(|| format!("PR #{number} not found or gh not authenticated."))?;
        let files = prs::fetch_pr_files(commands, number, repo);
        if files.is_empty() {
            return Ok(format!(
                "PR #{number}: no changed files found (may require gh auth)."
            ));
        }
        let (comms, nodes) = prs::compute_pr_impact(&files, g)?;
        let empty = vec![];
        let ci = prs::parse_ci(pr["statusCheckRollup"].as_array().unwrap_or(&empty));
        let review = pr["reviewDecision"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("none");
        let author = pr["author"]["login"].as_str().unwrap_or("?");
        let mut lines = vec![
            format!("PR #{number}: {}", text(&pr["title"])),
            format!("CI: {ci}  Review: {review}"),
            format!("Base: {}  Author: {author}", text(&pr["baseRefName"])),
            format!(
                "\nGraph impact: {nodes} nodes across {} communities",
                comms.len()
            ),
            format!(
                "Communities touched: {}",
                crate::python_compat::repr(&serde_json::json!(comms))
            ),
            format!("Files changed ({}):", files.len()),
        ];
        lines.extend(files.iter().take(20).map(|f| format!("  {f}")));
        if files.len() > 20 {
            lines.push(format!("  … and {} more", files.len() - 20));
        }
        return Ok(lines.join("\n"));
    }
    let base = args["base"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| prs::detect_default_branch(commands, repo));
    let mut prs = prs::fetch_prs(commands, repo, Some(&base), 100)?;
    let worktrees = prs::fetch_worktrees(commands);
    for pr in &mut prs {
        pr.worktree_path = worktrees.get(&pr.branch).cloned();
    }
    if name == "list_prs" {
        return Ok(prs::format_prs_text(&prs, &base, now));
    }
    prs.retain(|p| p.base_branch == base && !matches!(p.status(now), "WRONG-BASE" | "STALE"));
    if prs.is_empty() {
        return Ok(format!("No actionable PRs targeting {base}."));
    }
    for chunk in prs.chunks_mut(8) {
        std::thread::scope(|scope| {
            let tasks = chunk
                .iter()
                .map(|p| {
                    let n = p.number;
                    scope.spawn(move || prs::fetch_pr_files(commands, n, repo))
                })
                .collect::<Vec<_>>();
            for (pr, t) in chunk.iter_mut().zip(tasks) {
                let files = t.join().unwrap_or_default();
                if !files.is_empty() {
                    if let Ok((comms, nodes)) = prs::compute_pr_impact(&files, g) {
                        pr.communities_touched = comms;
                        pr.nodes_affected = nodes;
                    }
                    pr.files_changed = files;
                }
            }
        });
    }
    let header = format!(
        "Actionable PRs targeting {base}: {}\nRank these by review priority. Higher blast_radius = more graph communities affected = higher merge risk.\n",
        prs.len()
    );
    let mut lines = vec![header];
    prs.sort_by_key(|p| {
        prs::STATUS_ORDER
            .iter()
            .position(|s| *s == p.status(now))
            .unwrap_or(99)
    });
    for p in prs {
        let blast = p.blast_radius();
        let impact = if blast.is_empty() {
            String::new()
        } else {
            format!("  blast_radius={blast}")
        };
        let wt = p
            .worktree_path
            .as_ref()
            .filter(|s| !s.is_empty())
            .map(|s| format!("  worktree={s}"))
            .unwrap_or_default();
        lines.push(format!(
            "PR #{} [{}] CI={} review={} age={}d author={}{impact}{wt}\n  title: {}",
            p.number,
            p.status(now),
            p.ci_status,
            if p.review_decision.is_empty() {
                "none"
            } else {
                &p.review_decision
            },
            p.days_old(now),
            p.author,
            p.title
        ));
    }
    Ok(lines.join("\n\n"))
}
