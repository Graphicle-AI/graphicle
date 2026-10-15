// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/llm.py
//! Adaptive retry, deterministic corpus merging and incremental checkpoints.
use crate::{
    build::Diagnostics,
    cache::{ExtractionCache, Prompt, SemanticOptions},
    file_slice::{Unit, unit_path},
    llm::{self, Error, ErrorKind, Result, Runtime},
    llm_config as cfg, llm_parse as parse,
    python_compat::{scalar_key, string},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::Instant,
};
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ExtractOptions {
    pub backend: Option<String>,
    pub model: Option<String>,
    pub root: PathBuf,
    pub deep_mode: bool,
    #[serde(skip_serializing)]
    pub api_key: Option<String>,
}
impl Default for ExtractOptions {
    fn default() -> Self {
        Self {
            backend: None,
            model: None,
            root: PathBuf::from("."),
            deep_mode: false,
            api_key: None,
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CorpusOptions {
    #[serde(flatten)]
    pub extract: ExtractOptions,
    pub token_budget: Option<i64>,
    pub chunk_size: i64,
    pub max_concurrency: i64,
    pub max_retry_depth: Option<i64>,
    pub cache_root: Option<PathBuf>,
    pub use_tokenizer: bool,
}
impl Default for CorpusOptions {
    fn default() -> Self {
        Self {
            extract: ExtractOptions {
                backend: Some("kimi".into()),
                ..Default::default()
            },
            token_budget: Some(60_000),
            chunk_size: 20,
            max_concurrency: 4,
            max_retry_depth: None,
            cache_root: None,
            use_tokenizer: true,
        }
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct Progress {
    pub index: usize,
    pub total: usize,
    pub result: Value,
}
pub(crate) type Extractor<'a> =
    dyn Fn(&[Unit], &ExtractOptions, &mut Diagnostics) -> Result<Value> + Sync + 'a;
impl Runtime {
    pub fn adaptive(
        &self,
        chunk: &[Unit],
        o: &ExtractOptions,
        max_depth: i64,
        depth: i64,
        d: &mut Diagnostics,
    ) -> Result<Value> {
        self.adaptive_using(chunk, o, max_depth, depth, d, &|c, o, d| {
            self.extract_files(c, o, d)
        })
    }
    pub(crate) fn adaptive_using(
        &self,
        chunk: &[Unit],
        o: &ExtractOptions,
        max_depth: i64,
        depth: i64,
        d: &mut Diagnostics,
        call: &Extractor<'_>,
    ) -> Result<Value> {
        if chunk.is_empty() {
            return Err(Error::value("Cannot retry an empty chunk"));
        }
        let split_slice = || -> Option<(Vec<Unit>, Vec<Unit>)> {
            if chunk.len() != 1 || depth >= max_depth {
                return None;
            }
            let Unit::Slice(s) = &chunk[0] else {
                return None;
            };
            self.source
                .lock()
                .ok()?
                .source
                .bisect_slice(s)
                .map(|(a, b)| (vec![Unit::Slice(a)], vec![Unit::Slice(b)]))
        };
        let merge_two =
            |left: &[Unit], right: &[Unit], model: Value, d: &mut Diagnostics| -> Result<Value> {
                let left = self.adaptive_using(left, o, max_depth, depth + 1, d, call)?;
                let right = self.adaptive_using(right, o, max_depth, depth + 1, d, call)?;
                let mut v = parse::empty();
                parse::merge_into(&mut v, &left);
                parse::merge_into(&mut v, &right);
                v["model"] = model;
                v["finish_reason"] = json!("stop");
                v["_partial_files"] = json!(parse::merged_partial_files(&[&left, &right]));
                Ok(v)
            };
        let mut result = call(chunk, o, d);
        if max_depth > 0 {
            for delay in [2.0, 8.0] {
                if !result
                    .as_ref()
                    .is_ok_and(|r| r["finish_reason"].as_str() == Some("hollow"))
                {
                    break;
                }
                d.stderr.push(format!(
                    "[graphicle] retrying the same chunk of {} in {delay}s after a hollow response",
                    chunk.len()
                ));
                self.sleep(delay);
                result = call(chunk, o, d);
            }
        }
        let mut result = match result {
            Ok(v) => v,
            Err(e) => {
                let timed = e.kind == ErrorKind::Timeout;
                if !timed && !llm::context_exceeded(&e.message) {
                    return Err(e);
                }
                let reason = if timed {
                    "timed out"
                } else {
                    "exceeded context"
                };
                if chunk.len() == 1 {
                    if let Some((left, right)) = split_slice() {
                        d.stderr.push(format!("[graphicle] slice of {} {reason} at depth {depth}; splitting the slice and retrying",unit_path(&chunk[0]).display()));
                        return merge_two(&left, &right, json!(o.model), d);
                    }
                    d.stderr.push(format!(
                        "[graphicle] single-file chunk {} {} and cannot be split further: {e}",
                        unit_path(&chunk[0]).display(),
                        if timed {
                            "timed out"
                        } else {
                            "exceeds model context"
                        }
                    ));
                    return Ok(empty_result(o));
                }
                if depth >= max_depth {
                    d.stderr.push(format!("[graphicle] chunk of {} {} at recursion depth {depth} (max {max_depth}) — dropping",chunk.len(),if timed{"still times out"}else{"still overflows context"}));
                    return Ok(empty_result(o));
                }
                d.stderr.push(format!("[graphicle] chunk of {} {reason} at depth {depth} ({:?}); splitting in half and retrying",chunk.len(),e.kind));
                let mid = chunk.len() / 2;
                return merge_two(&chunk[..mid], &chunk[mid..], json!(o.model), d);
            }
        };
        if result["finish_reason"].as_str() == Some("hollow") {
            d.stderr.push(format!("[graphicle] chunk of {} still hollow after {} attempt(s) — giving up on this chunk. Its files are marked for re-extraction on the next run. A hollow response usually means a rate limit, a transport hiccup, a refusal, or a model that answered in prose rather than JSON.",chunk.len(),if max_depth>0{3}else{1}));
            mark_chunk_partial(&mut result, chunk);
            result["finish_reason"] = json!("stop");
            return Ok(result);
        }
        if result["finish_reason"].as_str() != Some("length") {
            return Ok(result);
        }
        if chunk.len() == 1 {
            if let Some((left, right)) = split_slice() {
                d.stderr.push(format!("[graphicle] slice of {} truncated at depth {depth}; splitting the slice and retrying",unit_path(&chunk[0]).display()));
                return merge_two(&left, &right, json!(o.model), d);
            }
            d.stderr.push(format!("[graphicle] single-file chunk {} truncated at max_completion_tokens — partial result kept (not cached as complete)",unit_path(&chunk[0]).display()));
            mark_chunk_partial(&mut result, chunk);
            return Ok(result);
        }
        if depth >= max_depth {
            d.stderr.push(format!("[graphicle] chunk of {} still truncated at recursion depth {depth} (max {max_depth}) — partial result kept (not cached as complete)",chunk.len()));
            mark_chunk_partial(&mut result, chunk);
            return Ok(result);
        }
        let mid = chunk.len() / 2;
        d.stderr.push(format!("[graphicle] chunk of {} truncated at depth {depth}, splitting into halves of {mid} and {}",chunk.len(),chunk.len()-mid));
        merge_two(&chunk[..mid], &chunk[mid..], result["model"].clone(), d)
    }
    pub fn extract_corpus(
        &self,
        files: &[Unit],
        o: &CorpusOptions,
        cache: &mut ExtractionCache,
        d: &mut Diagnostics,
        on_done: &mut dyn FnMut(Progress),
    ) -> Result<Value> {
        self.corpus_using(files, o, cache, d, on_done, &|c, o, d| {
            self.extract_files(c, o, d)
        })
    }
    pub(crate) fn corpus_using(
        &self,
        files: &[Unit],
        o: &CorpusOptions,
        cache: &mut ExtractionCache,
        d: &mut Diagnostics,
        on_done: &mut dyn FnMut(Progress),
        call: &Extractor<'_>,
    ) -> Result<Value> {
        let chunks = {
            let mut source = self
                .source
                .lock()
                .map_err(|_| Error::runtime("source cache lock poisoned"))?;
            let files = source.expand(files).map_err(Error::value)?;
            if let Some(b) = o.token_budget {
                source
                    .pack(&files, b, o.use_tokenizer)
                    .map_err(Error::value)?
            } else {
                if o.chunk_size == 0 {
                    return Err(Error::value("range() arg 3 must not be zero"));
                }
                if o.chunk_size < 0 {
                    vec![]
                } else {
                    files
                        .chunks(o.chunk_size as usize)
                        .map(|s| s.to_vec())
                        .collect()
                }
            }
        };
        let total = chunks.len();
        let workers = self.worker_count(
            o.extract.backend.as_deref().unwrap_or("kimi"),
            o.max_concurrency,
            total,
        );
        let max_depth = o
            .max_retry_depth
            .unwrap_or_else(|| cfg::max_retry_depth(&self.config.env, 3));
        let mut merged = json!({"nodes":[],"edges":[],"hyperedges":[],"input_tokens":0,"output_tokens":0,"failed_chunks":0});
        let mut results = BTreeMap::new();
        let run = |index: usize| {
            let mut d = Diagnostics::default();
            let start = Instant::now();
            let mut result =
                self.adaptive_using(&chunks[index], &o.extract, max_depth, 0, &mut d, call);
            if let Ok(v) = &mut result {
                v["elapsed_seconds"] =
                    json!((start.elapsed().as_secs_f64() * 100.0).round() / 100.0)
            }
            (index, result, d)
        };
        let mut accept = |(index, result, mut logs): (usize, Result<Value>, Diagnostics)| {
            d.stdout.append(&mut logs.stdout);
            d.stderr.append(&mut logs.stderr);
            match result {
                Ok(result) => {
                    if workers == 1 {
                        parse::merge_into(&mut merged, &result)
                    }
                    if cfg::get(&self.config.env, "GRAPHICLE_NO_INCREMENTAL_CACHE").is_empty() {
                        checkpoint(&result, &chunks[index], o, cache, d)
                    }
                    on_done(Progress {
                        index,
                        total,
                        result: result.clone(),
                    });
                    if workers > 1 {
                        results.insert(index, result);
                    }
                }
                Err(e) => {
                    d.stderr.push(format!(
                        "[graphicle] chunk {}/{total} failed: {e}",
                        index + 1
                    ));
                    let n = merged["failed_chunks"].as_u64().unwrap_or(0) + 1;
                    merged["failed_chunks"] = json!(n)
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
                    let run = &run;
                    let next = &next;
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
        for result in results.values() {
            parse::merge_into(&mut merged, result)
        }
        if merged["failed_chunks"].as_u64().unwrap_or(0) > 0 {
            d.stderr.push(format!("[graphicle] WARNING: {}/{total} semantic chunk(s) failed — see errors above. Partial results returned.",merged["failed_chunks"]))
        }
        reconcile(&mut merged, &chunks, &o.extract.root, d);
        llm::stamp_semantic_origin(&mut merged);
        Ok(merged)
    }
}
fn empty_result(o: &ExtractOptions) -> Value {
    json!({"nodes":[],"edges":[],"hyperedges":[],"input_tokens":0,"output_tokens":0,"model":o.model,"finish_reason":"stop"})
}
fn mark_chunk_partial(v: &mut Value, chunk: &[Unit]) {
    parse::mark_partial(v);
    let mut files: BTreeSet<_> = parse::merged_partial_files(&[v]).into_iter().collect();
    files.extend(parse::chunk_partial_files(chunk));
    v["_partial_files"] = json!(files);
}
fn checkpoint(
    result: &Value,
    chunk: &[Unit],
    o: &CorpusOptions,
    cache: &mut ExtractionCache,
    d: &mut Diagnostics,
) {
    let warnings_before = cache.warnings.len();
    let partial = parse::partial_source_files(result);
    let options = SemanticOptions {
        root: o.extract.root.clone(),
        cache_root: o.cache_root.clone(),
        mode: o.extract.deep_mode.then(|| "deep".into()),
        prompt: Some(Prompt::Text(llm::extraction_system(o.extract.deep_mode))),
        prompt_file: None,
        merge_existing: true,
        allowed_source_files: Some(
            chunk
                .iter()
                .map(|u| unit_path(u).to_string_lossy().into_owned())
                .collect(),
        ),
        partial_source_files: (!partial.is_empty()).then_some(partial),
    };
    if let Err(e) = cache.save_semantic(
        result["nodes"].as_array().map(Vec::as_slice).unwrap_or(&[]),
        result["edges"].as_array().map(Vec::as_slice).unwrap_or(&[]),
        result["hyperedges"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[]),
        &options,
    ) {
        d.stderr.push(format!(
            "[graphicle] incremental cache checkpoint failed: {e}"
        ))
    }
    d.stderr
        .extend(cache.warnings[warnings_before..].iter().cloned());
}
pub fn reconcile(merged: &mut Value, chunks: &[Vec<Unit>], root: &Path, d: &mut Diagnostics) {
    let dispatched: BTreeSet<PathBuf> = chunks
        .iter()
        .flatten()
        .map(|u| unit_path(u).to_owned())
        .collect();
    let resolved: HashSet<PathBuf> = dispatched
        .iter()
        .map(|p| llm::resolved_against_root(&p.to_string_lossy(), root))
        .collect();
    let out_of_scope = |n: &Value| {
        n["source_file"]
            .as_str()
            .filter(|s| !s.is_empty())
            .is_some_and(|sf| {
                let p = llm::resolved_against_root(sf, root);
                p.is_file() && !resolved.contains(&p)
            })
    };
    let mut ids = BTreeSet::new();
    let mut kept_ids = BTreeSet::new();
    let mut files = BTreeSet::new();
    let mut kept = vec![];
    let mut dropped = 0;
    for n in merged["nodes"].as_array().into_iter().flatten() {
        if out_of_scope(n) {
            dropped += 1;
            if !n["id"].is_null()
                && let Some(k) = scalar_key(&n["id"])
            {
                ids.insert(k);
            }
            files.insert(string(&n["source_file"]));
        } else {
            if !n["id"].is_null()
                && let Some(k) = scalar_key(&n["id"])
            {
                kept_ids.insert(k);
            }
            kept.push(n.clone())
        }
    }
    merged["out_of_scope_dropped"] = json!(dropped);
    if dropped > 0 {
        merged["nodes"] = json!(kept);
        // An edge referencing a dropped node goes. One attributed to an
        // undispatched file stays when both endpoints are kept nodes (#2276);
        // hyperedges stay on the out-of-scope predicate.
        if let Some(edges) = merged["edges"].as_array_mut() {
            edges.retain(|e| {
                let kept_end = |k: &str| scalar_key(&e[k]).is_some_and(|id| kept_ids.contains(&id));
                !["source", "target"]
                    .iter()
                    .any(|k| scalar_key(&e[*k]).is_some_and(|id| ids.contains(&id)))
                    && (!out_of_scope(e) || (kept_end("source") && kept_end("target")))
            })
        }
        if let Some(hs) = merged["hyperedges"].as_array_mut() {
            hs.retain(|h| {
                !out_of_scope(h)
                    && !h["nodes"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|v| scalar_key(v).is_some_and(|id| ids.contains(&id)))
            })
        }
        let mut names: Vec<_> = files
            .iter()
            .map(|p| {
                Path::new(p)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        let shown = names.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
        let more = if files.len() > 5 {
            format!(" (+{} more)", files.len() - 5)
        } else {
            String::new()
        };
        d.stderr.push(format!("[graphicle] WARNING: dropped {dropped} out-of-scope node(s) attributed to file(s) not dispatched for extraction: {shown}{more}. The model mis-attributed them to another corpus file; they were excluded from the graph (#1895)."));
    }
    let covered: HashSet<_> = merged["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|n| n["source_file"].as_str().filter(|s| !s.is_empty()))
        .map(|sf| llm::resolved_against_root(sf, root))
        .collect();
    let uncovered: Vec<_> = dispatched
        .iter()
        .filter(|p| !crate::fsutil::resolve_missing(p).is_ok_and(|p| covered.contains(&p)))
        .collect();
    merged["uncovered_files"] = json!(
        uncovered
            .iter()
            .map(|p| p.to_string_lossy())
            .collect::<Vec<_>>()
    );
    if !uncovered.is_empty() {
        let shown = uncovered
            .iter()
            .take(5)
            .map(|p| p.file_name().unwrap_or_default().to_string_lossy())
            .collect::<Vec<_>>()
            .join(", ");
        let more = if uncovered.len() > 5 {
            format!(" (+{} more)", uncovered.len() - 5)
        } else {
            String::new()
        };
        d.stderr.push(format!("[graphicle] WARNING: {}/{} dispatched file(s) produced no nodes and are absent from the graph: {shown}{more}. The model returned a response but omitted them; a re-run will retry them.",uncovered.len(),dispatched.len()));
    }
}
