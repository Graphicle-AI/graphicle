// Adapted from Graphify 0.9.95 (92741d16), Apache-2.0; see NOTICE.md.
// Upstream: graphify/prs.py
//! Graph-aware pull-request status, impact and command orchestration.
pub use crate::prs_io::{
    Commands, Completed, NativeCommands, PR_JSON_FIELDS, attach_graph_impact,
    detect_default_branch, fetch_pr, fetch_pr_files, fetch_prs, fetch_worktrees, gh,
    load_graph_json, parse_pr_dict,
};
pub use crate::prs_render::{Output, RenderContext};
pub use crate::prs_triage::{NativeTriage, Triage, resolve_triage_backend, triage_with_opus};
use crate::{Result, graph::Graph, paths, python_compat, python_datetime};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

pub const STATUS_ORDER: &[&str] = &[
    "WRONG-BASE",
    "CI-FAIL",
    "CHANGES-REQ",
    "DRAFT",
    "STALE",
    "PENDING",
    "APPROVED",
    "READY",
];
pub const HELP: &str = include_str!("assets/prs/help.txt");
#[derive(Clone, Debug)]
pub struct Timestamp {
    pub text: String,
    pub micros: i64,
}
impl Timestamp {
    /// The current time, as `datetime.now(timezone.utc)`.
    pub fn now() -> Self {
        let now = python_datetime::DateTime::now();
        Self {
            text: now.isoformat(),
            micros: now.timestamp_micros(),
        }
    }
    pub fn parse(text: &str) -> Result<Self> {
        let date = python_datetime::parse(text)
            .ok_or_else(|| format!("Invalid PR update datetime: {text}"))?;
        Ok(Self {
            text: text.into(),
            micros: date.timestamp_micros(),
        })
    }
}
impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.text)
    }
}
impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        Self::parse(&text).map_err(serde::de::Error::custom)
    }
}
fn default_base() -> String {
    "main".into()
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PRInfo {
    pub number: i64,
    pub title: String,
    pub branch: String,
    pub base_branch: String,
    pub author: String,
    pub is_draft: bool,
    pub review_decision: String,
    pub ci_status: String,
    pub updated_at: Timestamp,
    #[serde(default = "default_base")]
    pub expected_base: String,
    #[serde(default)]
    pub worktree_path: Option<String>,
    #[serde(default)]
    pub communities_touched: Vec<i64>,
    #[serde(default)]
    pub nodes_affected: i64,
    #[serde(default)]
    pub files_changed: Vec<String>,
}
impl PRInfo {
    pub fn days_old(&self, now_micros: i64) -> i64 {
        (now_micros - self.updated_at.micros).div_euclid(86_400_000_000)
    }
    pub fn status(&self, now: i64) -> &'static str {
        classify(self, &self.expected_base, now)
    }
    pub fn blast_radius(&self) -> String {
        let n = self.nodes_affected;
        if n == 0 {
            return String::new();
        }
        let c = self.communities_touched.len();
        format!(
            "{n} node{} / {c} communit{}",
            if n == 1 { "" } else { "s" },
            if c == 1 { "y" } else { "ies" }
        )
    }
}
pub fn classify(pr: &PRInfo, base: &str, now: i64) -> &'static str {
    if pr.base_branch != base {
        "WRONG-BASE"
    } else if pr.ci_status == "FAILURE" {
        "CI-FAIL"
    } else if pr.review_decision == "CHANGES_REQUESTED" {
        "CHANGES-REQ"
    } else if pr.is_draft {
        "DRAFT"
    } else if pr.days_old(now) >= 14 {
        "STALE"
    } else if pr.review_decision == "APPROVED" {
        "APPROVED"
    } else if pr.ci_status == "PENDING" {
        "PENDING"
    } else {
        "READY"
    }
}
/// `_parse_ci`. `statusCheckRollup` mixes two entry types: a CheckRun (GitHub
/// Actions and other check apps) carries status/conclusion, and a legacy
/// StatusContext commit status (Jenkins, CircleCI) carries only `state`, one
/// of SUCCESS, FAILURE, ERROR, PENDING or EXPECTED. Both are read, a failing
/// status first (Graphify 0.9.93, #4533).
pub fn parse_ci(rollup: &[Value]) -> &'static str {
    let conclusions = rollup
        .iter()
        .map(|r| {
            let conclusion = &r["conclusion"];
            if crate::build_normalize::truthy(conclusion) {
                conclusion
            } else {
                &r["state"]
            }
        })
        .collect::<Vec<_>>();
    let any = |names: &[&str]| {
        conclusions
            .iter()
            .any(|c| c.as_str().is_some_and(|s| names.contains(&s)))
    };
    if any(&[
        "FAILURE",
        "CANCELLED",
        "TIMED_OUT",
        "ACTION_REQUIRED",
        "STARTUP_FAILURE",
        "ERROR",
    ]) {
        "FAILURE"
    } else if rollup
        .iter()
        .any(|r| matches!(r["status"].as_str(), Some("IN_PROGRESS" | "QUEUED")))
        || any(&["PENDING", "EXPECTED"])
    {
        "PENDING"
    } else if any(&["SUCCESS"]) {
        "SUCCESS"
    } else {
        "NONE"
    }
}
pub fn path_match(graph_src: &str, pr_file: &str) -> bool {
    graph_src == pr_file
        || graph_src.ends_with(&format!("/{pr_file}"))
        || pr_file.ends_with(&format!("/{graph_src}"))
}
pub(crate) fn integer(value: &Value) -> Result<i64> {
    if let Some(i) = value.as_i64() {
        return Ok(i);
    }
    if let Some(v) = value.as_bool() {
        return Ok(i64::from(v));
    }
    if let Some(s) = value.as_str() {
        return Ok(crate::llm_config::parse_integer(s)
            .ok_or_else(|| format!("Invalid integer: {}", python_compat::repr(value)))?);
    }
    value
        .as_f64()
        .filter(|v| v.is_finite() && *v >= i64::MIN as f64 && *v < i64::MAX as f64)
        .map(|v| v.trunc() as i64)
        .ok_or_else(|| "Invalid community number".into())
}
#[derive(Default)]
pub struct ImpactIndex {
    files: Vec<(String, BTreeSet<i64>, i64)>,
}
impl ImpactIndex {
    pub fn from_nodes<'a>(nodes: impl Iterator<Item = &'a Value>) -> Result<Self> {
        let mut indices = BTreeMap::new();
        let mut files: Vec<(String, BTreeSet<i64>, i64)> = Vec::new();
        for node in nodes {
            let src = node.get("source_file").filter(|v| crate::build::truthy(v));
            let Some(src) = src else { continue };
            let src = src.as_str().ok_or("Graph source_file must be a string")?;
            let next = files.len();
            let i = *indices.entry(src.to_owned()).or_insert_with(|| {
                files.push((src.into(), BTreeSet::new(), 0));
                next
            });
            if !node["community"].is_null() {
                files[i].1.insert(integer(&node["community"])?);
            }
            files[i].2 += 1;
        }
        Ok(Self { files })
    }
    pub fn impact(&self, files: &[String]) -> (Vec<i64>, i64) {
        let mut matched = BTreeSet::new();
        let mut communities = BTreeSet::new();
        let mut nodes = 0;
        for f in files {
            for (i, (source, cs, count)) in self.files.iter().enumerate() {
                if !matched.contains(&i) && path_match(source, f) {
                    matched.insert(i);
                    communities.extend(cs);
                    nodes += count;
                }
            }
        }
        (communities.into_iter().collect(), nodes)
    }
}
pub fn compute_pr_impact(files: &[String], graph: &Graph) -> Result<(Vec<i64>, i64)> {
    let data: Vec<_> = graph
        .nodes
        .iter()
        .map(|n| Value::Object(n.attributes.to_map()))
        .collect();
    Ok(ImpactIndex::from_nodes(data.iter())?.impact(files))
}
pub fn build_community_labels(data: &Value, top_n: i64) -> Result<Value> {
    let mut result = serde_json::Map::new();
    for n in data["nodes"].as_array().into_iter().flatten() {
        if n["community"].is_null() {
            continue;
        }
        let label = n
            .get("label")
            .filter(|v| crate::build::truthy(v))
            .or_else(|| n.get("id").filter(|v| crate::build::truthy(v)));
        if let Some(label) = label {
            let key = integer(&n["community"])?.to_string();
            result
                .entry(key)
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .unwrap()
                .push(label.clone());
        }
    }
    for labels in result.values_mut() {
        let a = labels.as_array_mut().unwrap();
        let limit = if top_n < 0 {
            (a.len() as i64 + top_n).max(0) as usize
        } else {
            top_n as usize
        };
        a.truncate(limit);
    }
    Ok(Value::Object(result))
}
pub(crate) fn ordered<'a>(prs: &'a [PRInfo], base: &str, now: i64) -> Vec<&'a PRInfo> {
    let mut p: Vec<_> = prs.iter().filter(|p| p.base_branch == base).collect();
    p.sort_by_key(|p| {
        (
            STATUS_ORDER
                .iter()
                .position(|s| *s == p.status(now))
                .unwrap_or(99),
            p.days_old(now),
        )
    });
    p
}
pub fn format_prs_text(prs: &[PRInfo], base: &str, now: i64) -> String {
    let actionable = ordered(prs, base, now);
    let wrong = prs.len() - actionable.len();
    let mut lines = vec![format!(
        "Open PRs targeting {base}: {}  ({wrong} on wrong base, not shown)\n",
        actionable.len()
    )];
    for p in actionable {
        let radius = p.blast_radius();
        let impact = if radius.is_empty() {
            String::new()
        } else {
            format!("  blast_radius={radius}")
        };
        lines.push(format!(
            "#{} [{}] CI={} review={} age={}d author={}{impact}\n  {}",
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
    lines.join("\n\n")
}
pub fn cmd_prs(
    args: &[String],
    commands: &dyn Commands,
    triage: &dyn Triage,
    context: &RenderContext,
) -> Result<Output> {
    let mut base = None;
    let mut repo = None;
    let (mut do_triage, mut worktrees, mut conflicts, mut wrong) = (false, false, false, false);
    let mut number = None;
    let mut graph_path = PathBuf::from(paths::default_graph_json());
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "--triage" => do_triage = true,
            "--worktrees" => worktrees = true,
            "--conflicts" => conflicts = true,
            "--wrong-base" => wrong = true,
            "--base" | "-b" if i + 1 < args.len() => {
                i += 1;
                base = Some(args[i].clone());
            }
            "--repo" | "-R" if i + 1 < args.len() => {
                i += 1;
                repo = Some(args[i].clone());
            }
            "--graph" if i + 1 < args.len() => {
                i += 1;
                graph_path = PathBuf::from(&args[i]);
            }
            "-h" | "--help" => {
                return Ok(Output {
                    stdout: format!("{}\n", crate::cli::program_text(HELP)),
                    ..Default::default()
                });
            }
            _ => {
                if let Some(v) = arg.strip_prefix("--base=") {
                    base = Some(v.into());
                } else if let Some(v) = arg.strip_prefix("--graph=") {
                    graph_path = PathBuf::from(v);
                } else {
                    let n = arg.trim_start_matches('#');
                    if !n.is_empty() && n.chars().all(crate::dedup_strings::decimal) {
                        number = crate::llm_config::parse_integer(n);
                    }
                }
            }
        }
        i += 1;
    }
    let base = base.unwrap_or_else(|| detect_default_branch(commands, repo.as_deref()));
    let mut prs = match fetch_prs(commands, repo.as_deref(), Some(&base), 50) {
        Ok(p) => p,
        Err(e) => {
            return Ok(Output {
                stderr: format!("{}\n", context.color("31", &format!("  Error: {e}"))),
                code: 1,
                ..Default::default()
            });
        }
    };
    let tree_map = fetch_worktrees(commands);
    for pr in &mut prs {
        pr.worktree_path = tree_map.get(&pr.branch).cloned();
    }
    if let Some(n) = number {
        // A PR beyond the open list is fetched on its own (#3829), and the
        // graph impact is computed for that PR only, wrong base included.
        let mut found = prs.iter().find(|p| p.number == n).cloned();
        if found.is_none() {
            found = fetch_pr(commands, n, repo.as_deref(), Some(&base))?;
            if let Some(pr) = found.as_mut().filter(|pr| !pr.branch.is_empty()) {
                pr.worktree_path = tree_map.get(&pr.branch).cloned();
            }
        }
        let Some(mut pr) = found else {
            return Ok(Output {
                stderr: format!(
                    "{}\n",
                    context.color("31", &format!("  PR #{n} not found in open PRs."))
                ),
                code: 1,
                ..Default::default()
            });
        };
        if graph_path.exists() {
            attach_graph_impact(
                commands,
                std::slice::from_mut(&mut pr),
                &graph_path,
                repo.as_deref(),
                context.now_micros,
                None,
                true,
            )?;
        }
        return Ok(Output {
            stdout: crate::prs_render::render_pr_detail(&pr, context),
            ..Default::default()
        });
    }
    // Graph impact is expensive (concurrent `gh pr diff` calls): only triage
    // and conflict detection need it.
    let labels = if graph_path.exists() && (do_triage || conflicts) {
        attach_graph_impact(
            commands,
            &mut prs,
            &graph_path,
            repo.as_deref(),
            context.now_micros,
            None,
            false,
        )?
    } else {
        json!({})
    };
    let mut out = Output::default();
    if do_triage {
        out.stdout = crate::prs_render::render_dashboard(&prs, &base, wrong, context);
        let tail = triage_with_opus(&prs, &base, triage, context);
        out.stdout.push_str(&tail.stdout);
        out.stderr.push_str(&tail.stderr);
        out.code = tail.code;
    } else if worktrees {
        out.stdout = crate::prs_render::render_worktrees(&prs, &tree_map, context);
    } else {
        out.stdout = crate::prs_render::render_dashboard(&prs, &base, wrong, context);
        if conflicts {
            out.stdout.push_str(&crate::prs_render::render_conflicts(
                &prs, &base, &labels, context,
            ));
        }
    }
    Ok(out)
}
