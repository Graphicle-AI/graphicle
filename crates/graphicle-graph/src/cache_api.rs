// Adapted from Graphify 0.9.91 (10130e79), Apache-2.0; see NOTICE.md.
// Upstream: graphify/cache.py
//! Cache boundary for embedded callers and development comparisons.
use crate::{Result, cache, cache::*, python_compat};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};
static CACHE: OnceLock<Mutex<ExtractionCache>> = OnceLock::new();
pub fn flush() {
    if let Some(c) = CACHE.get()
        && let Ok(mut c) = c.lock()
    {
        c.flush()
    }
}
fn path(r: &Value, k: &str, default: &str) -> PathBuf {
    PathBuf::from(r.get(k).and_then(Value::as_str).unwrap_or(default))
}
fn prompt(v: &Value) -> Result<Option<Prompt>> {
    if v.is_null() {
        Ok(None)
    } else if let Some(s) = v.as_str() {
        Ok(Some(Prompt::Text(s.into())))
    } else {
        Ok(serde_json::from_value(v.clone())
            .map(Some)
            .map_err(crate::Error::from)?)
    }
}
fn options(r: &Value) -> Result<CacheOptions> {
    Ok(CacheOptions {
        root: path(r, "root", "."),
        kind: r["kind"].as_str().unwrap_or("ast").into(),
        cache_root: r["cache_root"].as_str().map(PathBuf::from),
        prompt: prompt(&r["prompt"])?,
        prompt_file: r["prompt_file"].as_str().map(PathBuf::from),
        allow_legacy: r["allow_legacy"].as_bool().unwrap_or(true),
        allow_partial: r["allow_partial"].as_bool().unwrap_or(false),
        salt: r["salt"].as_str().map(str::to_owned),
    })
}
fn strings(v: &Value) -> Option<Vec<String>> {
    v.as_array()
        .map(|a| a.iter().map(python_compat::string).collect())
}
fn semantic_options(r: &Value) -> Result<SemanticOptions> {
    Ok(SemanticOptions {
        root: path(r, "root", "."),
        cache_root: r["cache_root"].as_str().map(PathBuf::from),
        mode: r["mode"].as_str().map(str::to_owned),
        prompt: prompt(&r["prompt"])?,
        prompt_file: r["prompt_file"].as_str().map(PathBuf::from),
        merge_existing: r["merge_existing"].as_bool().unwrap_or(false),
        allowed_source_files: strings(&r["allowed_source_files"]),
        partial_source_files: strings(&r["partial_source_files"]),
    })
}
fn run(c: &mut ExtractionCache, r: &Value) -> Result<Value> {
    let name = r["function"].as_str().unwrap_or("");
    let a = &r["arguments"];
    let p = path(a, "path", ".");
    let root = path(a, "root", ".");
    let txt = |k: &str| a[k].as_str().unwrap_or("");
    Ok(match name {
        "snapshot" => serde_json::to_value(&*c).unwrap(),
        "_cleanup_stale_ast_entries" => {
            c.cleanup_stale(&path(a, "ast_base", "."), &path(a, "current_dir", "."));
            Value::Null
        }
        "prompt_fingerprint" => {
            let p = prompt(&a["prompt"])?.ok_or("Expected prompt")?;
            json!(match p {
                Prompt::Text(t) => cache::prompt_fingerprint(&t),
                Prompt::File(p) => cache::prompt_fingerprint(
                    &crate::fsutil::read_text_lossy(&p).map_err(crate::Error::from)?
                ),
            })
        }
        "_resolve_prompt_fp" => json!(c.resolve_prompt(
            prompt(&a["prompt"])?.as_ref(),
            a["prompt_file"].as_str().map(Path::new)
        )),
        "_salted_key" => json!(cache::salted_key(txt("h"), a["salt"].as_str())),
        "_grammar_fingerprint" => json!(
            c.grammar_fingerprint
                .clone()
                .unwrap_or_else(|| cache::grammar_fingerprint().to_string())
        ),
        "_target_file_present" => json!(cache::target_file_present(txt("target"), &root)),
        "_present_target_files" => json!(cache::present_target_files(&a["result"], &root)),
        "_body_content" => {
            let bytes = ar(&a["content"])
                .iter()
                .map(|v| {
                    v.as_u64()
                        .and_then(|n| u8::try_from(n).ok())
                        .ok_or("Expected byte")
                })
                .collect::<std::result::Result<Vec<_>, _>>()?;
            json!(cache::body_content(&bytes))
        }
        "_mtime_granularity_ns" => json!(c.granularity()),
        "_stat_sig_fresh" => json!(stat_fresh(
            &a["entry"],
            serde_json::from_value(a["st"].clone()).map_err(crate::Error::from)?,
            c.granularity()
        )),
        "_stat_entry_for" => json!(c.stat_entry(
            txt("abs_key"),
            serde_json::from_value(a["st"].clone()).map_err(crate::Error::from)?,
            a["observed_at_ns"].as_i64().ok_or("Expected timestamp")?
        )),
        "_stat_key_to_relative" => json!(stat_relative(txt("key"), &path(a, "anchor", "."))),
        "_stat_key_to_absolute" => json!(stat_absolute(txt("key"), &path(a, "anchor", "."))),
        "_stat_index_file" => json!(ps(&c.stat_file(&root)?)),
        "_ensure_stat_index" => {
            c.ensure_index(&root, a["cache_root"].as_str().map(Path::new))?;
            Value::Null
        }
        "_flush_stat_index" => {
            c.flush();
            Value::Null
        }
        "_normalize_path" => json!(ps(&normalize(&p))),
        "file_hash" => json!(c.file_hash(&p, &root, a["cache_root"].as_str().map(Path::new))?),
        "word_count_lookup" => serde_json::to_value(c.word_count_lookup(
            &p,
            &root,
            a["cache_root"].as_str().map(Path::new),
        )?)
        .unwrap(),
        "complete_word_count" => c.complete_word_count(
            serde_json::from_value(a["ticket"].clone()).map_err(crate::Error::from)?,
            a["value"].clone(),
        )?,
        "cached_word_count" => c.cached_word_count(
            &p,
            &root,
            |p| {
                let t = crate::fsutil::read_text_lossy(p).map_err(crate::Error::from)?;
                Ok(json!(
                    t.split(|c: char| c.is_whitespace() || matches!(c, '\x1c'..='\x1f'))
                        .filter(|s| !s.is_empty())
                        .count()
                ))
            },
            a["cache_root"].as_str().map(Path::new),
        )?,
        "_normalize_source_file_value" => {
            json!(normalize_source(txt("src"), &path(a, "root_resolved", ".")))
        }
        "_semantic_entry_matches_path" => json!(semantic_matches(&a["result"], &p, &root)?),
        "_id_anchor" => json!(id_anchor(txt("path_str"), txt("rel_str"))),
        "_portability_anchors" => json!(anchors(&p, &root)),
        "_relativize_source_files_in"
        | "_absolutize_source_files_in"
        | "_relativize_ids_in"
        | "_absolutize_ids_in" => {
            let mut v = a["payload"].clone();
            match name {
                "_relativize_source_files_in" => relativize_sources(&mut v, &root),
                "_absolutize_source_files_in" => absolutize_sources(&mut v, &root),
                "_relativize_ids_in" => relativize_ids(&mut v, &p, &root),
                _ => absolutize_ids(&mut v, &p, &root),
            }
            v
        }
        "_rewrite_strings" | "_rewrite_id_keyed_table_keys" => {
            let mut v = a["payload"].clone();
            let f = |s: &str| s.replace(txt("from"), txt("to"));
            if name == "_rewrite_strings" {
                rewrite_strings(&mut v, &f)
            } else {
                rewrite_table_keys(&mut v, &f)
            }
            v
        }
        "source_identity" => json!(ps(&SourceMatcher::new(&root)?.identity(txt("value"))?)),
        "source_normalize" => json!(SourceMatcher::new(&root)?.normalized(txt("value"))),
        "cache_dir" => json!(ps(&c.cache_dir(
            &root,
            a["kind"].as_str().unwrap_or("ast"),
            a["prompt_fp"].as_str()
        )?)),
        "load_cached" => json!(c.load(&p, &options(a)?)?),
        "save_cached" => {
            c.save(&p, &a["result"], &options(a)?)?;
            Value::Null
        }
        "cached_files" => json!(c.cached_files(&root)?),
        "clear_cache" => {
            c.clear(&root)?;
            Value::Null
        }
        "prune_semantic_cache" => json!(
            c.prune(
                &root,
                &strings(&a["live_hashes"])
                    .unwrap_or_default()
                    .into_iter()
                    .collect()
            )?
        ),
        "check_semantic_cache" => c.check_semantic(
            &strings(&a["files"]).unwrap_or_default(),
            &semantic_options(a)?,
        )?,
        "_group_has_partial_marker" => json!(group_partial(&a["group"])),
        "save_semantic_cache" => json!(c.save_semantic(
            ar(&a["nodes"]),
            ar(&a["edges"]),
            ar(&a["hyperedges"]),
            &semantic_options(a)?
        )?),
        "scope_semantic_result" => {
            let mut result = a["result"].clone();
            let (dropped, count) = scope(
                &mut result,
                &root,
                strings(&a["allowed_source_files"]).as_deref(),
            )?;
            json!({"result":result,"dropped_source_files":dropped,"dropped_item_count":count})
        }
        _ => return Err(format!("unknown cache function: {name}").into()),
    })
}
fn response(c: &mut ExtractionCache, r: &Value) -> Value {
    let before = c.warnings.len();
    let result = run(c, r);
    let warnings = c.warnings[before..].to_vec();
    let state = serde_json::to_value(&*c).unwrap();
    match result {
        Ok(value) => json!({"value":value,"state":state,"warnings":warnings}),
        Err(error) => json!({"error":error,"state":state,"warnings":warnings}),
    }
}
pub fn dispatch(r: &Value) -> Result<Value> {
    if let Some(state) = r.get("state") {
        let mut c: ExtractionCache =
            serde_json::from_value(state.clone()).map_err(crate::Error::from)?;
        return Ok(response(&mut c, r));
    }
    let mut c = CACHE
        .get_or_init(|| Mutex::new(ExtractionCache::default()))
        .lock()
        .map_err(crate::Error::from)?;
    Ok(response(&mut c, r))
}
