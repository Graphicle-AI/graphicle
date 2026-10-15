// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/llm.py
//! Batched community names with bounded recovery and usage accounting.
use crate::{
    build::Diagnostics,
    cluster::Communities,
    graph::Graph,
    llm::{Error, ErrorKind, Result, Runtime, Usage},
    llm_config::{self as cfg, trim},
    llm_source::cap,
    python_compat::{repr_string, scalar_key, string},
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    sync::{
        LazyLock,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
};
pub type Labels = Map<String, Value>;
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LabelOptions {
    pub backend: Option<String>,
    pub model: Option<String>,
    pub gods: Vec<Value>,
    pub max_communities: Option<i64>,
    pub top_k: i64,
    pub batch_size: i64,
    pub max_concurrency: i64,
    pub quiet: bool,
}
impl Default for LabelOptions {
    fn default() -> Self {
        Self {
            backend: None,
            model: None,
            gods: vec![],
            max_communities: None,
            top_k: 12,
            batch_size: 100,
            max_concurrency: 4,
            quiet: false,
        }
    }
}
pub fn placeholders(communities: &Communities) -> Labels {
    communities
        .iter()
        .map(|(c, _)| (c.to_string(), json!(format!("Community {c}"))))
        .collect()
}
pub fn lines(
    graph: &Graph,
    communities: &Communities,
    gods: &[Value],
    max_communities: Option<i64>,
    top_k: i64,
) -> (Vec<String>, Vec<i64>) {
    let gods: BTreeSet<_> = gods
        .iter()
        .filter_map(|g| scalar_key(if g.is_object() { &g["id"] } else { g }))
        .collect();
    let mut ordered: Vec<_> = communities.iter().collect();
    ordered.sort_by_key(|(_, ns)| std::cmp::Reverse(ns.len()));
    let len = ordered.len() as i64;
    ordered.truncate(
        max_communities
            .map(|n| if n < 0 { (len + n).max(0) } else { n.min(len) })
            .unwrap_or(len) as usize,
    );
    let (mut lines, mut cids) = (vec![], vec![]);
    for (cid, members) in ordered {
        let mut ranked: Vec<_> = members.iter().collect();
        ranked.sort_by_key(|n| !scalar_key(n).is_some_and(|s| gods.contains(&s)));
        let (mut names, mut seen) = (vec![], HashSet::new());
        for id in ranked {
            let label = graph
                .node_index(id)
                .and_then(|i| graph.nodes[i].attributes.get("label"))
                .unwrap_or(id);
            let label = string(label);
            let label = cap(trim(&label).trim_matches(['(', ')']), 60);
            if !label.is_empty() && seen.insert(label.to_lowercase()) {
                names.push(label)
            }
            if names.len() as i64 >= top_k {
                break;
            }
        }
        if !names.is_empty() {
            lines.push(format!("{cid}: {}", names.join(", ")));
            cids.push(*cid)
        }
    }
    (lines, cids)
}
pub fn parse_labels(text: &str, cids: &[i64]) -> Result<Labels> {
    static FENCE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?i)^\s*```(?:json)?\s*|\s*```\s*$").unwrap());
    let mut cleaned = FENCE.replace_all(trim(text), "").into_owned();
    if !cleaned.starts_with('{')
        && let (Some(start), Some(end)) = (cleaned.find('{'), cleaned.rfind('}'))
        && end > start
    {
        cleaned = cleaned[start..=end].into()
    }
    let data = if let Some(m) = serde_json::from_str::<Value>(&cleaned)
        .ok()
        .and_then(|v| v.as_object().cloned())
    {
        m
    } else {
        static PAIRS: LazyLock<regex::Regex> = LazyLock::new(|| {
            regex::Regex::new(r#""?(-?\d+)"?\s*:\s*"([^"\\]*(?:\\.[^"\\]*)*)""#).unwrap()
        });
        let pairs: Map<String, Value> = PAIRS
            .captures_iter(&cleaned)
            .map(|c| (c[1].into(), json!(&c[2])))
            .collect();
        if pairs.is_empty() {
            return Err(Error::value(format!(
                "label response is not parseable JSON: {}",
                repr_string(&cap(text, 120))
            )));
        }
        pairs
    };
    let mut out = Labels::new();
    for cid in cids {
        if let Some(name) = data
            .get(&cid.to_string())
            .and_then(Value::as_str)
            .map(trim)
            .filter(|s| !s.is_empty())
        {
            out.insert(cid.to_string(), json!(name));
        }
    }
    Ok(out)
}
#[derive(Clone, Copy)]
pub struct BatchOptions<'a> {
    pub backend: &'a str,
    pub model: Option<&'a str>,
    pub depth: i64,
    pub max_depth: i64,
}
impl Runtime {
    pub fn label_batch(
        &self,
        cids: &[i64],
        lines: &[String],
        options: BatchOptions<'_>,
        usage: &mut Usage,
        d: &mut Diagnostics,
    ) -> Result<Labels> {
        if cids.len() != lines.len() {
            return Err(Error::value(
                "Each community must have one labeling input line",
            ));
        }
        if cids.is_empty() {
            return Ok(Labels::new());
        }
        let prompt = format!(
            "{}{}",
            include_str!("assets/llm/label_prompt.txt"),
            lines.join("\n")
        );
        let max = cfg::max_tokens(&self.config.env, (256 + 48 * cids.len() as i64).min(8192));
        let result = self
            .call(&prompt, options.backend, options.model, max, usage, d)
            .and_then(|text| parse_labels(&text, cids));
        let terminal = cids.len() <= 1 || options.depth >= options.max_depth;
        let (mut labels, missing, missing_lines) = match result {
            Ok(labels) if labels.len() == cids.len() || terminal => return Ok(labels),
            Ok(labels) => {
                let (missing, lines): (Vec<_>, Vec<_>) = cids
                    .iter()
                    .zip(lines)
                    .filter(|(id, _)| !labels.contains_key(&id.to_string()))
                    .map(|(id, line)| (*id, line.clone()))
                    .unzip();
                (labels, missing, lines)
            }
            Err(error) if error.kind == ErrorKind::Value && !terminal => {
                (Labels::new(), cids.to_vec(), lines.to_vec())
            }
            Err(error) => {
                if error.kind == ErrorKind::Value {
                    d.stderr.push(format!("[graphicle label] batch of {} still unparseable at depth {} (cids={:?}{}): {error}", cids.len(), options.depth, &cids[..cids.len().min(5)], if cids.len()>5 { "..." } else { "" }));
                }
                return Err(error);
            }
        };
        let mid = (missing.len() / 2).max(1);
        let next = BatchOptions {
            depth: options.depth + 1,
            ..options
        };
        let mut parse_failure = None;
        for range in [0..mid, mid..missing.len()] {
            if !range.is_empty() {
                match self.label_batch(
                    &missing[range.clone()],
                    &missing_lines[range],
                    next,
                    usage,
                    d,
                ) {
                    Ok(recovered) => labels.extend(recovered),
                    Err(error) if error.kind == ErrorKind::Value => {
                        parse_failure.get_or_insert(error);
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        if labels.is_empty()
            && let Some(error) = parse_failure
        {
            return Err(error);
        }
        Ok(labels)
    }
    pub fn label_communities(
        &self,
        g: &Graph,
        cs: &Communities,
        o: &LabelOptions,
        usage: &mut Usage,
        d: &mut Diagnostics,
    ) -> Result<Labels> {
        let backend = o
            .backend
            .as_deref()
            .ok_or_else(|| Error::value("A labeling backend is required"))?;
        let mut labels = placeholders(cs);
        let (lines, cids) = lines(g, cs, &o.gods, o.max_communities, o.top_k);
        if lines.is_empty() {
            return Ok(labels);
        }
        if o.batch_size <= 0 {
            return Err(Error::value("batch_size must be positive"));
        }
        let size = o.batch_size as usize;
        let total = cids.len().div_ceil(size);
        let workers = self.worker_count(backend, o.max_concurrency, total);
        let mut written = 0;
        let mut errors = BTreeMap::new();
        let run = |i: usize| {
            let start = i * size;
            let end = (start + size).min(cids.len());
            let mut d = Diagnostics::default();
            let mut usage = Usage::default();
            let result = self.label_batch(
                &cids[start..end],
                &lines[start..end],
                BatchOptions {
                    backend,
                    model: o.model.as_deref(),
                    depth: 0,
                    max_depth: 3,
                },
                &mut usage,
                &mut d,
            );
            (i, result, usage, d)
        };
        let mut accept =
            |(index, result, used, mut logs): (usize, Result<Labels>, Usage, Diagnostics)| {
                usage.add(&used);
                d.stdout.append(&mut logs.stdout);
                d.stderr.append(&mut logs.stderr);
                match result {
                    Ok(v) => {
                        written += v.len();
                        labels.extend(v)
                    }
                    Err(e) => {
                        d.stderr.push(format!(
                            "[graphicle label] batch {}/{total} ({} communities) failed: {e}",
                            index + 1,
                            size.min(cids.len() - index * size)
                        ));
                        errors.insert(index, e);
                    }
                }
            };
        if workers == 1 {
            for i in 0..total {
                accept(run(i))
            }
        } else {
            let next = AtomicUsize::new(0);
            let (sender, receiver) = mpsc::channel();
            std::thread::scope(|scope| {
                for _ in 0..workers {
                    let sender = sender.clone();
                    let next = &next;
                    let run = &run;
                    scope.spawn(move || {
                        loop {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            if i >= total {
                                break;
                            }
                            if sender.send(run(i)).is_err() {
                                break;
                            }
                        }
                    });
                }
                drop(sender);
                for result in receiver {
                    accept(result)
                }
            });
        }
        if written == 0
            && let Some((_, e)) = errors.into_iter().next()
        {
            return Err(e);
        }
        Ok(labels)
    }
    pub fn generate_labels(
        &self,
        g: &Graph,
        cs: &Communities,
        o: &LabelOptions,
        usage: &mut Usage,
        d: &mut Diagnostics,
    ) -> (Labels, String) {
        let mut o = o.clone();
        if o.backend.is_none() {
            o.backend = self.config.detect_backend(d).ok().flatten()
        }
        if o.backend.as_ref().is_none_or(|s| s.is_empty())
            && cfg::claude_command(&self.config.env).is_some()
        {
            o.backend = Some("claude-cli".into())
        }
        if o.backend.as_ref().is_none_or(|s| s.is_empty()) {
            if !o.quiet {
                d.stderr.push("[graphicle label] no LLM backend configured; keeping Community N placeholders. Set an API key (e.g. GOOGLE_API_KEY) or pass --backend.".into())
            }
            return (placeholders(cs), "placeholder".into());
        }
        match self.label_communities(g, cs, &o, usage, d) {
            Ok(labels) => {
                let named = placeholders(cs)
                    .iter()
                    .filter(|(id, fallback)| labels.get(*id) != Some(*fallback))
                    .count();
                if named < cs.len() && !o.quiet {
                    d.stderr.push(format!("[graphicle label] warning: labeled {named} of {} communities; {} kept structural fallback names.", cs.len(), cs.len() - named));
                }
                (labels, "llm".into())
            }
            Err(e) => {
                if !o.quiet {
                    d.stderr.push(format!("[graphicle label] warning: community labeling failed ({e}); using Community N placeholders."))
                }
                (placeholders(cs), "placeholder".into())
            }
        }
    }
}
