//! Source-grounded invalidation adapted from Hindsight's retraction handling.
//! A missing dependency withholds current reuse; it never erases original history.
use crate::{Result, string};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// Most memories (any state) one repository's listing and dependency passes
/// cover. Recall ranks at most `retrieval::MAX_RECALL_MEMORIES` of them.
pub const MAX_MEMORIES: usize = 100_000;

pub fn retractions(request: &Value) -> Result<Value> {
    let based_on = request
        .get("based_on")
        .and_then(Value::as_object)
        .ok_or("Expected grounding citations by fact type")?;
    let live: BTreeSet<String> = serde_json::from_value(
        request
            .get("live_ids")
            .cloned()
            .ok_or("Expected live IDs")?,
    )
    .map_err(|_| "Invalid live IDs")?;
    if based_on.len() > 10000 || live.len() > 20000 {
        return Err("Grounding limit exceeded".into());
    }
    let mut seen = BTreeSet::new();
    let mut cited = Vec::new();
    let mut missing = Vec::new();
    let mut total = 0;
    for (kind, values) in based_on {
        if !["world", "experience", "observation"].contains(&kind.as_str()) {
            continue;
        }
        let Some(items) = values.as_array() else {
            continue;
        };
        if items.len() > 10000 {
            return Err("Too many grounding citations".into());
        }
        for item in items {
            let id = item.get("id").and_then(Value::as_str).unwrap_or("");
            if id.is_empty() {
                continue;
            }
            total += 1;
            if seen.insert(id.to_owned()) {
                cited.push(id);
            }
            if !live.contains(id) {
                missing.push(item.clone());
                continue;
            }
        }
    }
    let drop_ids: BTreeSet<_> = missing
        .iter()
        .filter_map(|x| x.get("id").and_then(Value::as_str))
        .collect();
    let pruned: BTreeMap<_, _> = based_on
        .iter()
        .filter_map(|(kind, values)| {
            values.as_array().map(|items| {
                (
                    kind.clone(),
                    items
                        .iter()
                        .filter(|item| {
                            !item
                                .get("id")
                                .and_then(Value::as_str)
                                .is_some_and(|id| drop_ids.contains(id))
                        })
                        .cloned()
                        .collect::<Vec<_>>(),
                )
            })
        })
        .collect();
    let unresolvable = total > 0 && total == missing.len();
    Ok(
        json!({"cited_ids":cited,"retracted":missing,"pruned":pruned,
        "cited_memory_count":total,"unresolvable":unresolvable,
        "requires_review":!missing.is_empty(),"original_preserved":true}),
    )
}

/// Deepest parent chain a walk follows before reporting `dependency_depth_exceeded`.
const MAX_DEPTH: usize = 100;
/// Root id `dependencies_all` gives each node's walk; no node may use it.
const ROOT: &str = "root";

/// A parent edge is followed only to the reviewed revision it was built on.
fn followed(record: &Value, expected: &Value) -> bool {
    record.get("revision") == Some(expected) && record["state"] == "reviewed"
}

/// One depth-first dependency walk from a root. `root` stands in for the
/// reserved root id when the walk runs on behalf of `dependencies_all`.
struct Walk<'a> {
    by_id: &'a BTreeMap<&'a str, &'a Value>,
    root: Option<&'a Value>,
    visiting: BTreeSet<String>,
    complete: BTreeSet<String>,
    reasons: BTreeSet<String>,
}

impl<'a> Walk<'a> {
    fn new(by_id: &'a BTreeMap<&'a str, &'a Value>, root: Option<&'a Value>) -> Self {
        Self {
            by_id,
            root,
            visiting: BTreeSet::new(),
            complete: BTreeSet::new(),
            reasons: BTreeSet::new(),
        }
    }

    fn lookup(&self, id: &str) -> Option<&'a Value> {
        match self.root {
            Some(root) if id == ROOT => Some(root),
            _ => self.by_id.get(id).copied(),
        }
    }

    fn visit(&mut self, id: &str, depth: usize) -> Result<()> {
        if self.complete.contains(id) {
            return Ok(());
        }
        if depth > MAX_DEPTH {
            self.reasons.insert("dependency_depth_exceeded".into());
            return Ok(());
        }
        if !self.visiting.insert(id.into()) {
            self.reasons.insert(format!("dependency_cycle:{id}"));
            return Ok(());
        }
        if let Some(node) = self.lookup(id) {
            if let Some(items) = node.get("reasons").and_then(Value::as_array) {
                for reason in items {
                    self.reasons.insert(string_or_error(reason)?);
                }
            }
            if let Some(parents) = node.get("parents").and_then(Value::as_object) {
                for (parent, expected) in parents {
                    match self.lookup(parent) {
                        Some(record) if followed(record, expected) => {
                            self.visit(parent, depth + 1)?;
                        }
                        _ => {
                            self.reasons.insert(format!("decision_changed:{parent}"));
                        }
                    }
                }
            }
        } else {
            self.reasons.insert(format!("dependency_missing:{id}"));
        }
        self.visiting.remove(id);
        self.complete.insert(id.into());
        Ok(())
    }
}

pub fn dependencies(request: &Value) -> Result<Value> {
    let nodes = request
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or("Expected dependency records")?;
    let root = string(request, "root")?;
    // The root plus every memory of a repository.
    if nodes.len() > MAX_MEMORIES + 1 {
        return Err("Dependency closure limit exceeded".into());
    }
    let mut by_id = BTreeMap::new();
    for node in nodes {
        if by_id.insert(string(node, "id")?, node).is_some() {
            return Err("Duplicate dependency identity".into());
        }
    }
    let mut walk = Walk::new(&by_id, None);
    walk.visit(root, 0)?;
    let reasons = walk.reasons;
    Ok(
        json!({"stale":!reasons.is_empty(),"reasons":reasons,"visited":walk.complete,
        "automatic_deletion":false,"complete":!reasons.contains("dependency_depth_exceeded")}),
    )
}

/// `dependencies` for every node at once. For each node, the reasons equal
/// what `dependencies` reports for a root named `root` that carries the
/// node's parents and reasons, walked alongside all the nodes. Listing n
/// records that way costs n full walks; here acyclic closures within the
/// depth limit share one memoized pass, and only cycles, deeper chains and
/// references to the reserved root id take the single-root walk, so every
/// result is identical.
pub fn dependencies_all(request: &Value) -> Result<Value> {
    let nodes = request
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or("Expected dependency records")?;
    if nodes.len() > MAX_MEMORIES {
        return Err("Dependency closure limit exceeded".into());
    }
    let mut index: BTreeMap<&str, usize> = BTreeMap::new();
    let mut by_id: BTreeMap<&str, &Value> = BTreeMap::new();
    for (i, node) in nodes.iter().enumerate() {
        let id = string(node, "id")?;
        if id == ROOT {
            return Err("The root id is reserved".into());
        }
        if index.insert(id, i).is_some() {
            return Err("Duplicate dependency identity".into());
        }
        by_id.insert(id, node);
    }
    let count = nodes.len();
    // Each node's own reasons plus parents it cannot follow, and the parents
    // it follows. `shared` is cleared where the memoized pass cannot be used.
    let mut closure: Vec<BTreeSet<String>> = Vec::with_capacity(count);
    let mut edges: Vec<Vec<usize>> = Vec::with_capacity(count);
    let mut shared = vec![true; count];
    for (i, node) in nodes.iter().enumerate() {
        let mut reasons = BTreeSet::new();
        if let Some(items) = node.get("reasons").and_then(Value::as_array) {
            for reason in items {
                reasons.insert(string_or_error(reason)?);
            }
        }
        let mut followed_parents = Vec::new();
        if let Some(parents) = node.get("parents").and_then(Value::as_object) {
            for (parent, expected) in parents {
                if parent == ROOT {
                    shared[i] = false;
                    continue;
                }
                match index.get(parent.as_str()) {
                    Some(&p) if followed(&nodes[p], expected) => followed_parents.push(p),
                    _ => {
                        reasons.insert(format!("decision_changed:{parent}"));
                    }
                }
            }
        }
        closure.push(reasons);
        edges.push(followed_parents);
    }
    // Iterative post-order pass: a node's closure is its own reasons joined
    // with its followed parents' closures. A parent still on the stack means
    // a cycle; a height above the limit means the walk would stop early.
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        New,
        Active,
        Done,
    }
    let mut mark = vec![Mark::New; count];
    let mut height = vec![0usize; count];
    for start in 0..count {
        if mark[start] != Mark::New {
            continue;
        }
        mark[start] = Mark::Active;
        let mut stack = vec![(start, 0usize)];
        while let Some(top) = stack.last_mut() {
            let (node, next) = *top;
            if let Some(&parent) = edges[node].get(next) {
                top.1 += 1;
                if mark[parent] == Mark::New {
                    mark[parent] = Mark::Active;
                    stack.push((parent, 0));
                }
                continue;
            }
            stack.pop();
            let mut reasons = std::mem::take(&mut closure[node]);
            let mut tallest = 0;
            for &parent in &edges[node] {
                if mark[parent] != Mark::Done {
                    shared[node] = false;
                    continue;
                }
                shared[node] &= shared[parent];
                tallest = tallest.max(height[parent] + 1);
                reasons.extend(closure[parent].iter().cloned());
            }
            if tallest > MAX_DEPTH {
                shared[node] = false;
            }
            height[node] = tallest;
            closure[node] = reasons;
            mark[node] = Mark::Done;
        }
    }
    let mut out = serde_json::Map::with_capacity(count);
    for (i, node) in nodes.iter().enumerate() {
        let reasons = if shared[i] {
            std::mem::take(&mut closure[i])
        } else {
            let root = json!({"id":ROOT,"revision":0,"state":"reviewed",
                "parents":node.get("parents").cloned().unwrap_or(json!({})),
                "reasons":node.get("reasons").cloned().unwrap_or(json!([]))});
            let mut walk = Walk::new(&by_id, Some(&root));
            walk.visit(ROOT, 0)?;
            walk.reasons
        };
        out.insert(string(node, "id")?.to_owned(), json!(reasons));
    }
    Ok(json!({"reasons":out,"automatic_deletion":false}))
}
fn string_or_error(value: &Value) -> Result<String> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| "Invalid dependency reason".into())
}

/// MemOS-style prioritized task readiness, without a second queue or database.
/// The host supplies the canonical current revision and persisted job records.
pub fn readiness(request: &Value) -> Result<Value> {
    let version = string(request, "snapshot")?;
    let jobs = request
        .get("jobs")
        .and_then(Value::as_array)
        .ok_or("Expected projection jobs")?;
    if jobs.len() > 10000 {
        return Err("Projection job limit exceeded".into());
    }
    let budget = crate::number(request, "budget")?;
    let mut ready = Vec::new();
    let mut waiting = Vec::new();
    for job in jobs {
        let id = string(job, "id")?;
        let status = string(job, "status")?;
        if !["ready", "queued", "running", "failed", "missing"].contains(&status) {
            return Err("Invalid projection status".into());
        }
        if status == "ready" && job.get("snapshot").and_then(Value::as_str) == Some(version) {
            ready.push(id.to_owned());
        } else {
            waiting.push(job.clone());
        }
    }
    waiting.sort_by_key(|job| {
        (
            std::cmp::Reverse(job.get("priority").and_then(Value::as_u64).unwrap_or(0)),
            job.get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
        )
    });
    let mut used = 0usize;
    let mut scheduled = Vec::new();
    for job in &waiting {
        if job["status"] == "running" {
            continue;
        }
        let cost = crate::number(job, "cost")?;
        if cost <= budget.saturating_sub(used) {
            used += cost;
            scheduled.push(job["id"].clone());
        }
    }
    Ok(
        json!({"ready":ready,"pending":waiting,"suggested":scheduled,"cost":used,
        "fully_ready":waiting.is_empty(),"automatic_execution":false}),
    )
}
